//! The comparison engine: normalization, hashing, diffing, and redaction.
//!
//! Comparison is hybrid (spec §7.1): normalize both responses, hash the
//! normalized form with `blake3`, and only generate a structural diff when the
//! hashes differ. All output surfaces are redacted before anything is logged.
//!
//! Submodules:
//! - [`jsonpath`] — the supported JSONPath subset and its parser.
//! - [`normalize`] — JSON normalization driven by merged contract rules.
//! - [`hash`] — `blake3` over the normalized representation.
//! - [`diff`] — bounded, redacted JSON-aware structural diff.
//! - [`headers`] — the optional `Set-Cookie` / `Location` dimensions.
//! - [`redact`] — header / JSON-path / query redaction.
//! - [`result`] — [`result::ComparisonResult`] and [`result::Difference`] types.

pub mod diff;
pub mod hash;
pub mod headers;
pub mod jsonpath;
pub mod normalize;
pub mod redact;
pub mod result;

use axum::http::HeaderMap;
use bytes::Bytes;
use serde_json::Value;
use url::Url;

use crate::contract::model::ComparisonRules;
use diff::DiffLimits;
use result::{ChangeKind, ComparisonResult, DiffKind, Difference, HeaderMismatch};

/// A buffered response captured for comparison.
#[derive(Debug, Clone)]
pub struct Captured {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: HeaderMap,
    /// The full (bounded) response body.
    pub body: Bytes,
    /// The URL of the request that produced this response, against which a
    /// relative `Location` is resolved (RFC 9110 §10.2.2). Always known on the
    /// proxy's data path — each side carries its *own* upstream URL, which is
    /// what lets a legacy `/next` and a new `https://new.example/next` compare
    /// equal. `None` only where no request URL exists (tests, benchmarks); a
    /// relative `Location` then takes the exact-string fallback.
    pub request_url: Option<Url>,
}

/// Compare a legacy and a new response per the merged contract rules.
///
/// Both bodies are assumed already buffered and within limits (the sampling and
/// size gates live on the data path, Phase 4). The result is hybrid: a hash
/// decides match/mismatch, and a structural diff is produced only on mismatch.
pub fn compare(
    rules: &ComparisonRules,
    limits: &DiffLimits,
    legacy: &Captured,
    new: &Captured,
) -> ComparisonResult {
    let status_match = !rules.compare_status || legacy.status == new.status;
    let header_mismatches = compare_headers(&rules.compare_headers, &legacy.headers, &new.headers);
    // The two optional dimensions: compared only when a contract layer declared
    // the block (spec §4.2). Absent = not compared at all, not "compared with
    // defaults".
    let (cookie_mismatches, cookies_truncated) = rules
        .set_cookie
        .as_ref()
        .map(|r| headers::compare_set_cookie(r, limits, &legacy.headers, &new.headers))
        .unwrap_or_default();
    let (location_mismatches, location_truncated) = rules
        .location
        .as_ref()
        .map(|r| {
            headers::compare_location(
                r,
                limits,
                &legacy.headers,
                legacy.request_url.as_ref(),
                &new.headers,
                new.request_url.as_ref(),
            )
        })
        .unwrap_or_default();

    let (body_match, diff_kind, differences, body_truncated) = if rules.compare_body {
        compare_bodies(rules, limits, &legacy.body, &new.body)
    } else {
        (true, None, Vec::new(), false)
    };
    // One flag for all three bounded surfaces (body diff, cookies, Location).
    let diff_truncated = body_truncated || cookies_truncated || location_truncated;

    ComparisonResult {
        status_match,
        legacy_status: legacy.status,
        new_status: new.status,
        body_match,
        diff_kind,
        differences,
        diff_truncated,
        header_mismatches,
        cookie_mismatches,
        location_mismatches,
    }
}

/// Compare bodies: structural JSON when both parse, opaque byte equality
/// otherwise.
fn compare_bodies(
    rules: &ComparisonRules,
    limits: &DiffLimits,
    legacy: &[u8],
    new: &[u8],
) -> (bool, Option<DiffKind>, Vec<Difference>, bool) {
    match (
        serde_json::from_slice::<Value>(legacy),
        serde_json::from_slice::<Value>(new),
    ) {
        (Ok(lv), Ok(nv)) => {
            let ln = normalize::normalize(&lv, &rules.json);
            let nn = normalize::normalize(&nv, &rules.json);
            if hash::hash_value(&ln) == hash::hash_value(&nn) {
                (true, None, Vec::new(), false)
            } else {
                match parse_redact_paths(&rules.json.redact_paths) {
                    Some(redact) => {
                        let (differences, truncated) = diff::diff(&ln, &nn, &redact, limits);
                        (false, Some(DiffKind::Json), differences, truncated)
                    }
                    // Fail closed: if any redact path is unparseable we cannot
                    // guarantee redaction, so report the mismatch but emit no
                    // diff values rather than risk leaking a secret. (Config
                    // validation makes this unreachable in practice.)
                    None => (false, Some(DiffKind::Json), Vec::new(), false),
                }
            }
        }
        _ => {
            // Non-JSON: compare raw bytes; record only a body-level mismatch
            // (byte counts, never the bytes themselves) so nothing leaks.
            if legacy == new {
                (true, Some(DiffKind::NonJson), Vec::new(), false)
            } else {
                let difference = Difference {
                    path: "$".to_string(),
                    kind: ChangeKind::Changed,
                    legacy: Some(Value::String(format!("<{} bytes, non-JSON>", legacy.len()))),
                    new: Some(Value::String(format!("<{} bytes, non-JSON>", new.len()))),
                };
                (false, Some(DiffKind::NonJson), vec![difference], false)
            }
        }
    }
}

/// Compare only the explicitly listed headers; sensitive values are redacted.
fn compare_headers(names: &[String], legacy: &HeaderMap, new: &HeaderMap) -> Vec<HeaderMismatch> {
    let mut mismatches = Vec::new();
    for name in names {
        let l = legacy.get(name).and_then(|v| v.to_str().ok());
        let n = new.get(name).and_then(|v| v.to_str().ok());
        if l != n {
            let sensitive = redact::SENSITIVE_HEADERS.contains(&name.to_ascii_lowercase().as_str());
            let mask = |v: Option<&str>| {
                v.map(|value| {
                    if sensitive {
                        redact::REDACTED.to_string()
                    } else {
                        value.to_string()
                    }
                })
            };
            mismatches.push(HeaderMismatch {
                name: name.clone(),
                legacy: mask(l),
                new: mask(n),
            });
        }
    }
    mismatches
}

/// Parse redact-path strings into validated [`jsonpath::JsonPath`]s. Returns
/// `None` if *any* path fails to parse, so the caller can fail closed rather
/// than redact with an incomplete set (paths are validated at load time, so
/// this is defensive).
fn parse_redact_paths(paths: &[String]) -> Option<Vec<jsonpath::JsonPath>> {
    paths.iter().map(|p| jsonpath::parse(p).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::model::{
        CookieValueMode, JsonRules, OriginMode, ResolvedLocationRules, ResolvedSetCookieRules,
    };

    fn captured(status: u16, body: &str) -> Captured {
        Captured {
            status,
            headers: HeaderMap::new(),
            body: Bytes::from(body.to_string()),
            request_url: None,
        }
    }

    fn rules() -> ComparisonRules {
        ComparisonRules::default()
    }

    #[test]
    fn equal_json_with_different_key_order_matches() {
        let legacy = captured(200, r#"{"a":1,"b":2}"#);
        let new = captured(200, r#"{"b":2,"a":1}"#);
        let result = compare(&rules(), &DiffLimits::default(), &legacy, &new);
        assert!(result.is_match());
        assert!(result.differences.is_empty());
    }

    #[test]
    fn body_mismatch_produces_diff() {
        let legacy = captured(200, r#"{"name":"A"}"#);
        let new = captured(200, r#"{"name":"B"}"#);
        let result = compare(&rules(), &DiffLimits::default(), &legacy, &new);
        assert!(!result.is_match());
        assert!(!result.body_match);
        assert_eq!(result.diff_kind, Some(DiffKind::Json));
        assert!(result.differences.iter().any(|d| d.path == "$.name"));
    }

    #[test]
    fn status_mismatch_detected_even_when_body_matches() {
        let legacy = captured(200, r#"{"ok":true}"#);
        let new = captured(404, r#"{"ok":true}"#);
        let result = compare(&rules(), &DiffLimits::default(), &legacy, &new);
        assert!(!result.status_match);
        assert!(result.body_match);
        assert!(!result.is_match());
    }

    #[test]
    fn ignored_field_does_not_cause_mismatch() {
        let mut r = rules();
        r.json = JsonRules {
            ignore_paths: vec!["$.ts".into()],
            ..Default::default()
        };
        let legacy = captured(200, r#"{"v":1,"ts":100}"#);
        let new = captured(200, r#"{"v":1,"ts":999}"#);
        assert!(compare(&r, &DiffLimits::default(), &legacy, &new).is_match());
    }

    #[test]
    fn non_json_bodies_compared_by_bytes_without_leaking() {
        let legacy = captured(200, "secret-plaintext-A");
        let new = captured(200, "secret-plaintext-B");
        let result = compare(&rules(), &DiffLimits::default(), &legacy, &new);
        assert!(!result.body_match);
        assert_eq!(result.diff_kind, Some(DiffKind::NonJson));
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains("secret-plaintext"));
    }

    #[test]
    fn compare_headers_only_when_listed_and_redacts_sensitive() {
        let mut r = rules();
        r.compare_headers = vec!["content-type".into(), "authorization".into()];
        let mut legacy = captured(200, "{}");
        let mut new = captured(200, "{}");
        legacy
            .headers
            .insert("content-type", "application/json".parse().unwrap());
        new.headers
            .insert("content-type", "text/plain".parse().unwrap());
        legacy
            .headers
            .insert("authorization", "Bearer legacy-secret".parse().unwrap());
        new.headers
            .insert("authorization", "Bearer new-secret".parse().unwrap());

        let result = compare(&r, &DiffLimits::default(), &legacy, &new);
        assert_eq!(result.header_mismatches.len(), 2);
        let serialized = serde_json::to_string(&result.header_mismatches).unwrap();
        assert!(serialized.contains("application/json")); // non-sensitive shown
        assert!(!serialized.contains("legacy-secret")); // authorization redacted
        assert!(!serialized.contains("new-secret"));
    }

    #[test]
    fn optional_dimensions_are_off_unless_a_contract_layer_declares_them() {
        let mut legacy = captured(200, "{}");
        let mut new = captured(200, "{}");
        legacy
            .headers
            .insert("set-cookie", "sid=abc; SameSite=Lax".parse().unwrap());
        new.headers
            .insert("set-cookie", "other=xyz; SameSite=None".parse().unwrap());
        legacy
            .headers
            .insert("location", "https://legacy.example/a".parse().unwrap());
        new.headers
            .insert("location", "https://new.example/b".parse().unwrap());

        // No `set_cookie` / `location` block: neither dimension is compared.
        let result = compare(&rules(), &DiffLimits::default(), &legacy, &new);
        assert!(result.is_match());
        assert!(result.cookie_mismatches.is_empty());
        assert!(result.location_mismatches.is_empty());

        // Declared: both dimensions now decide the verdict.
        let mut r = rules();
        r.set_cookie = Some(ResolvedSetCookieRules {
            compare: true,
            ignore_cookies: Vec::new(),
            ignore_attributes: Vec::new(),
            compare_values: CookieValueMode::Exact,
        });
        r.location = Some(ResolvedLocationRules {
            compare: true,
            ignore_query_params: Vec::new(),
            origin: OriginMode::Exact,
        });
        let result = compare(&r, &DiffLimits::default(), &legacy, &new);
        assert!(!result.is_match());
        // Two presence mismatches (one cookie per side) collapse into one kind.
        assert_eq!(result.cookie_mismatches.len(), 2);
        assert_eq!(
            result.mismatch_kinds(),
            ["location.origin", "location.path", "set_cookie.presence"]
        );
    }

    /// Limen safety invariant 5, for the new dimension: a `set_cookie` mismatch
    /// reports the cookie name and the attribute that differed, and **never**
    /// renders a cookie value — not for a value diff, not for a cookie only one
    /// side set, and not for an entry too malformed to parse.
    #[test]
    fn set_cookie_mismatches_never_render_a_cookie_value() {
        let mut r = rules();
        r.set_cookie = Some(ResolvedSetCookieRules {
            compare: true,
            ignore_cookies: Vec::new(),
            ignore_attributes: Vec::new(),
            compare_values: CookieValueMode::Exact,
        });
        let mut legacy = captured(200, "{}");
        let mut new = captured(200, "{}");
        for (headers, session, extra) in [
            (
                &mut legacy.headers,
                "session=legacy-secret-value; Path=/api; SameSite=Lax",
                "orphan=legacy-orphan-secret",
            ),
            (
                &mut new.headers,
                "session=new-secret-value; Path=/api; SameSite=None",
                "still-not-a-cookie-new-secret",
            ),
        ] {
            headers.append("set-cookie", session.parse().unwrap());
            headers.append("set-cookie", extra.parse().unwrap());
        }
        legacy
            .headers
            .append("set-cookie", "not-a-cookie-legacy-secret".parse().unwrap());
        // The same response redirects with an OAuth authorization code — a
        // credential in a query parameter, masked by the `location` dimension.
        r.location = Some(ResolvedLocationRules {
            compare: true,
            ignore_query_params: Vec::new(),
            origin: OriginMode::Exact,
        });
        legacy.headers.insert(
            "location",
            "https://app.example/cb?code=legacy-auth-code&next=/home"
                .parse()
                .unwrap(),
        );
        new.headers.insert(
            "location",
            "https://app.example/cb?code=new-auth-code&next=/home"
                .parse()
                .unwrap(),
        );

        let result = compare(&r, &DiffLimits::default(), &legacy, &new);
        assert!(!result.is_match());
        let serialized = serde_json::to_string(&result).unwrap();
        // Every secret-bearing spelling stays out of the rendered result…
        for secret in [
            "legacy-secret-value",
            "new-secret-value",
            "legacy-orphan-secret",
            "not-a-cookie-legacy-secret",
            "still-not-a-cookie-new-secret",
            "legacy-auth-code",
            "new-auth-code",
        ] {
            assert!(
                !serialized.contains(secret),
                "leaked {secret}: {serialized}"
            );
        }
        // …while the diff still says which cookie and which attribute differed,
        // and which query parameter carried the masked difference.
        assert!(serialized.contains("session"));
        assert!(serialized.contains("SameSite"));
        assert!(serialized.contains("Lax") && serialized.contains("None"));
        assert!(serialized.contains("\"param\":\"code\""));
    }
}
