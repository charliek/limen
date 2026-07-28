//! The `Set-Cookie` and `Location` comparison dimensions (spec §4.2; Pharos
//! §8.6).
//!
//! Both are *dimensions of their own*, not entries in the `compare_headers`
//! allowlist: `Set-Cookie` is multi-valued (a single-value header map would
//! silently drop all but one cookie), and `Location` needs URL semantics rather
//! than string equality. A route opts in by declaring the corresponding block in
//! its behavioral rules; with the block absent, neither dimension is compared at
//! all.
//!
//! Everything rendered here is safe to log: cookie *values* are never emitted
//! (only `<redacted>` / `<empty>` / `<present>` placeholders), and `Location`
//! query values are masked for the sensitive parameter names in
//! [`crate::compare::redact::SENSITIVE_QUERY_PARAMS`].

use std::collections::{BTreeMap, BTreeSet};

use axum::http::HeaderMap;
use url::Url;

use crate::compare::diff::DiffLimits;
use crate::compare::redact::{REDACTED, SENSITIVE_QUERY_PARAMS};
use crate::compare::result::{
    CookieMismatch, CookieMismatchKind, LocationMismatch, LocationMismatchKind, MALFORMED_COOKIE,
};
use crate::contract::model::{
    CookieValueMode, OriginMode, ResolvedLocationRules, ResolvedSetCookieRules,
};

const SET_COOKIE: &str = "set-cookie";
const LOCATION: &str = "location";

/// Rendered in place of a cookie value that exists but is empty — enough to
/// explain a `compare_values: presence` failure without showing anything.
const EMPTY: &str = "<empty>";
/// Rendered for a cookie one side set and the other did not.
const PRESENT: &str = "<present>";

/// Pair two sequences positionally, running to the longer side's length so an
/// unpaired tail surfaces as `(Some, None)` / `(None, Some)`. Both duplicate
/// cookie names and unparseable entries pair this way.
fn zip_longest<'a, T>(
    legacy: &'a [T],
    new: &'a [T],
) -> impl Iterator<Item = (Option<&'a T>, Option<&'a T>)> {
    (0..legacy.len().max(new.len())).map(|index| (legacy.get(index), new.get(index)))
}

/// A mismatch list bounded by [`DiffLimits::max_differences`], the same cap the
/// body diff obeys: a pathological response (hundreds of cookies, a query with
/// hundreds of parameters) can never grow an unbounded log line.
struct Bounded<T> {
    out: Vec<T>,
    max: usize,
    truncated: bool,
}

impl<T> Bounded<T> {
    fn new(limits: &DiffLimits) -> Self {
        Self {
            out: Vec::new(),
            max: limits.max_differences,
            truncated: false,
        }
    }

    /// Record a mismatch, or mark the list truncated once the cap is reached.
    fn push(&mut self, item: T) {
        if self.out.len() >= self.max {
            self.truncated = true;
        } else {
            self.out.push(item);
        }
    }

    fn finish(self) -> (Vec<T>, bool) {
        (self.out, self.truncated)
    }
}

/// Walk the union of two maps' keys in sorted order, carrying each side's value
/// (absent where that side has no entry). Cookie names, cookie attributes, and
/// `Location` query parameters are all paired by name this way; only the
/// "did they differ" test varies between them.
fn zip_by_key<'a, K: Ord, V>(
    legacy: &'a BTreeMap<K, V>,
    new: &'a BTreeMap<K, V>,
) -> impl Iterator<Item = (&'a K, Option<&'a V>, Option<&'a V>)> {
    let keys: BTreeSet<&K> = legacy.keys().chain(new.keys()).collect();
    keys.into_iter()
        .map(move |key| (key, legacy.get(key), new.get(key)))
}

// ---------------------------------------------------------------------------
// Set-Cookie
// ---------------------------------------------------------------------------

/// A parsed `Set-Cookie` value: `name=value` plus its attribute map.
struct Cookie<'a> {
    /// The cookie name, compared case-**sensitively** (RFC 6265).
    name: &'a str,
    /// The cookie value, never rendered into any output.
    value: &'a str,
    /// Attributes keyed by ASCII-lowercased name (attribute names are compared
    /// case-insensitively), holding the authored spelling and the value. A flag
    /// attribute (`Secure`, `HttpOnly`) carries an empty value.
    attributes: BTreeMap<String, (&'a str, &'a str)>,
}

/// Parse one `Set-Cookie` value. Returns `None` for a value that is not a
/// cookie at all — no `=` in the name/value pair, or an empty name (RFC 6265
/// §5.2, which discards such a Set-Cookie). Callers fall back to exact-string
/// comparison for those.
fn parse_cookie(raw: &str) -> Option<Cookie<'_>> {
    let mut parts = raw.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let mut attributes = BTreeMap::new();
    for part in parts {
        let (attr, value) = match part.split_once('=') {
            Some((attr, value)) => (attr.trim(), value.trim()),
            // A flag attribute (`Secure`) has a name but no value.
            None => (part.trim(), ""),
        };
        if attr.is_empty() {
            continue;
        }
        // A repeated attribute keeps its last occurrence, as RFC 6265 §5.2
        // prescribes for the attributes it defines.
        attributes.insert(attr.to_ascii_lowercase(), (attr, value));
    }
    Some(Cookie {
        name,
        value: value.trim(),
        attributes,
    })
}

/// Parse every `Set-Cookie` value on a response, splitting the well-formed
/// cookies from the raw bytes of the entries that could not be parsed.
fn collect_cookies(headers: &HeaderMap) -> (Vec<Cookie<'_>>, Vec<&[u8]>) {
    let mut cookies = Vec::new();
    let mut malformed = Vec::new();
    for value in headers.get_all(SET_COOKIE) {
        match value.to_str().ok().and_then(parse_cookie) {
            Some(cookie) => cookies.push(cookie),
            None => malformed.push(value.as_bytes()),
        }
    }
    (cookies, malformed)
}

/// Group parsed cookies by name, dropping the names the rules ignore. Duplicate
/// names keep response order, which is what makes their pairing positional.
fn group_by_name<'a, 'c>(
    cookies: &'c [Cookie<'a>],
    ignore: &[String],
) -> BTreeMap<&'a str, Vec<&'c Cookie<'a>>> {
    let mut groups: BTreeMap<&str, Vec<&Cookie>> = BTreeMap::new();
    for cookie in cookies {
        // Cookie names are case-sensitive (RFC 6265), so `ignore_cookies` is
        // matched exactly — unlike attribute names just below.
        if ignore.iter().any(|name| name == cookie.name) {
            continue;
        }
        groups.entry(cookie.name).or_default().push(cookie);
    }
    groups
}

/// How a cookie value is rendered: never the value itself.
fn render_value(value: &str) -> String {
    if value.is_empty() { EMPTY } else { REDACTED }.to_string()
}

/// Compare the `Set-Cookie` dimension of two responses. Returns the (bounded)
/// mismatches and whether the list was truncated at
/// [`DiffLimits::max_differences`].
pub fn compare_set_cookie(
    rules: &ResolvedSetCookieRules,
    limits: &DiffLimits,
    legacy: &HeaderMap,
    new: &HeaderMap,
) -> (Vec<CookieMismatch>, bool) {
    if !rules.compare {
        return (Vec::new(), false);
    }
    let (legacy_cookies, legacy_malformed) = collect_cookies(legacy);
    let (new_cookies, new_malformed) = collect_cookies(new);
    let legacy_groups = group_by_name(&legacy_cookies, &rules.ignore_cookies);
    let new_groups = group_by_name(&new_cookies, &rules.ignore_cookies);
    let ignored_attributes: Vec<String> = rules
        .ignore_attributes
        .iter()
        .map(|a| a.to_ascii_lowercase())
        .collect();

    let mut mismatches = Bounded::new(limits);
    for (name, legacy_group, new_group) in zip_by_key(&legacy_groups, &new_groups) {
        let legacy_group: &[&Cookie] = legacy_group.map_or(&[], Vec::as_slice);
        let new_group: &[&Cookie] = new_group.map_or(&[], Vec::as_slice);
        // Same-name cookies pair positionally within their group; a group that
        // runs out on one side leaves an unpaired cookie, i.e. a presence
        // mismatch.
        for pair in zip_longest(legacy_group, new_group) {
            match pair {
                (Some(l), Some(n)) => {
                    compare_cookie_pair(rules, &ignored_attributes, l, n, &mut mismatches)
                }
                (l, n) => mismatches.push(CookieMismatch {
                    name: name.to_string(),
                    kind: CookieMismatchKind::Presence,
                    attribute: None,
                    legacy: l.map(|_| PRESENT.to_string()),
                    new: n.map(|_| PRESENT.to_string()),
                }),
            }
        }
    }

    // Unparseable entries pair positionally with each other and fall back to
    // exact-string comparison — rendered as `<redacted>`, since an entry we
    // could not parse may still be carrying a secret.
    for (l, n) in zip_longest(&legacy_malformed, &new_malformed) {
        if l == n {
            continue;
        }
        mismatches.push(CookieMismatch {
            name: MALFORMED_COOKIE.to_string(),
            kind: CookieMismatchKind::Malformed,
            attribute: None,
            legacy: l.map(|_| REDACTED.to_string()),
            new: n.map(|_| REDACTED.to_string()),
        });
    }
    mismatches.finish()
}

/// Compare one paired cookie: its value (per `compare_values`) and its
/// attributes.
fn compare_cookie_pair(
    rules: &ResolvedSetCookieRules,
    ignored_attributes: &[String],
    legacy: &Cookie<'_>,
    new: &Cookie<'_>,
    out: &mut Bounded<CookieMismatch>,
) {
    let value_differs = match rules.compare_values {
        CookieValueMode::Exact => legacy.value != new.value,
        // `presence` asks only whether the two sides *agree* about a value
        // existing; the values themselves are never compared. Both empty is
        // agreement, not a failure — that is the cookie-deletion shape
        // (`session=; Max-Age=0`) both sides emit on logout.
        CookieValueMode::Presence => legacy.value.is_empty() != new.value.is_empty(),
    };
    if value_differs {
        out.push(CookieMismatch {
            name: legacy.name.to_string(),
            kind: CookieMismatchKind::Value,
            attribute: None,
            legacy: Some(render_value(legacy.value)),
            new: Some(render_value(new.value)),
        });
    }

    for (key, l, n) in zip_by_key(&legacy.attributes, &new.attributes) {
        if ignored_attributes.contains(key) {
            continue;
        }
        if let (Some((_, lv)), Some((_, nv))) = (l, n) {
            if lv == nv {
                continue;
            }
        }
        // Report the attribute under its authored spelling, preferring legacy's.
        let attribute = l.or(n).map(|(name, _)| (*name).to_string());
        out.push(CookieMismatch {
            name: legacy.name.to_string(),
            kind: CookieMismatchKind::Attribute,
            attribute,
            // Attribute values (`Path`, `SameSite`, `Domain`, …) carry no
            // secret, so they are shown verbatim — that is the whole point of
            // this mismatch.
            legacy: l.map(|(_, value)| (*value).to_string()),
            new: n.map(|(_, value)| (*value).to_string()),
        });
    }
}

// ---------------------------------------------------------------------------
// Location
// ---------------------------------------------------------------------------

/// A response's `Location` header in the three states comparison cares about.
struct Located<'a> {
    /// The raw header bytes, absent when the response sent no `Location`.
    raw: Option<&'a [u8]>,
    /// The resolved URL, absent when the header is missing *or* could not be
    /// resolved (which triggers the exact-string fallback).
    url: Option<Url>,
}

/// Resolve a response's `Location`, relative values against the URL of the
/// request that produced the response (RFC 9110 §10.2.2).
fn locate<'a>(headers: &'a HeaderMap, request_url: Option<&Url>) -> Located<'a> {
    let Some(value) = headers.get(LOCATION) else {
        return Located {
            raw: None,
            url: None,
        };
    };
    let raw = Some(value.as_bytes());
    let url = value.to_str().ok().and_then(|text| {
        // With no request URL (only reachable off the proxy's data path, where
        // it is always known), a relative Location cannot be resolved and takes
        // the exact-string fallback below.
        Url::options().base_url(request_url).parse(text).ok()
    });
    Located { raw, url }
}

/// A URL's origin as the three parts `origin: exact` compares: scheme, host,
/// and *effective* port (so `https://a` and `https://a:443` are one origin).
///
/// Deliberately **not** [`Url::origin`]: that returns an opaque, never-equal
/// origin for non-special schemes, which would make two identical
/// `mailto:`/`file:` Locations mismatch — and its JavaScript counterpart
/// (`URL.origin`) reports those differently again, which is exactly the kind of
/// cross-engine drift the lockstep obligation forbids.
fn origin_parts(url: &Url) -> (&str, Option<&str>, Option<u16>) {
    (url.scheme(), url.host_str(), url.port_or_known_default())
}

/// Render the compared origin parts. Schemes with no host (`mailto:`) render as
/// the bare scheme, which is all `origin_parts` compares for them.
fn render_origin(url: &Url) -> String {
    match origin_parts(url) {
        (scheme, Some(host), Some(port)) => format!("{scheme}://{host}:{port}"),
        (scheme, Some(host), None) => format!("{scheme}://{host}"),
        (scheme, None, _) => format!("{scheme}:"),
    }
}

/// Render a URL down to origin + path — enough to say *where* a side
/// redirected. Built from the compared parts rather than sliced out of the URL
/// (`..Position::AfterPath` would include `user:password@`), so credentials in
/// the userinfo are dropped, as is the query, which may carry tokens.
fn render_url(url: &Url) -> String {
    format!("{}{}", render_origin(url), url.path())
}

/// How one side is rendered in a presence mismatch: its target when resolvable,
/// `<redacted>` when it sent something unparseable, nothing when it sent no
/// `Location` at all.
fn rendered_side(side: &Located<'_>) -> Option<String> {
    match (&side.url, side.raw) {
        (Some(url), _) => Some(render_url(url)),
        (None, Some(_)) => Some(REDACTED.to_string()),
        (None, None) => None,
    }
}

/// The query as `name -> values`, minus the ignored parameter names.
fn query_map(url: &Url, ignore: &[String]) -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in url.query_pairs() {
        if ignore.iter().any(|i| i.as_str() == name.as_ref()) {
            continue;
        }
        map.entry(name.into_owned())
            .or_default()
            .push(value.into_owned());
    }
    map
}

/// Render one parameter's values, masking the ones whose *name* marks them
/// secret-bearing.
fn render_query(param: &str, values: Option<&Vec<String>>) -> Option<String> {
    let values = values?;
    if SENSITIVE_QUERY_PARAMS.contains(&param.to_ascii_lowercase().as_str()) {
        return Some(REDACTED.to_string());
    }
    Some(values.join(","))
}

/// Compare the `Location` dimension of two responses. `legacy_url` / `new_url`
/// are the URLs of the requests that produced each response, against which a
/// relative `Location` is resolved. Returns the (bounded) mismatches and whether
/// the list was truncated at [`DiffLimits::max_differences`].
pub fn compare_location(
    rules: &ResolvedLocationRules,
    limits: &DiffLimits,
    legacy: &HeaderMap,
    legacy_url: Option<&Url>,
    new: &HeaderMap,
    new_url: Option<&Url>,
) -> (Vec<LocationMismatch>, bool) {
    if !rules.compare {
        return (Vec::new(), false);
    }
    let l = locate(legacy, legacy_url);
    let n = locate(new, new_url);

    let single = |mismatch: LocationMismatch| (vec![mismatch], false);
    match (l.raw, n.raw) {
        // Neither side redirected: nothing to compare.
        (None, None) => (Vec::new(), false),
        // Exactly one side did: a presence mismatch.
        (None, Some(_)) | (Some(_), None) => single(LocationMismatch {
            kind: LocationMismatchKind::Presence,
            param: None,
            legacy: rendered_side(&l),
            new: rendered_side(&n),
        }),
        (Some(legacy_raw), Some(new_raw)) => match (l.url.as_ref(), n.url.as_ref()) {
            (Some(legacy_target), Some(new_target)) => {
                compare_targets(rules, limits, legacy_target, new_target)
            }
            // At least one side could not be resolved to a URL: exact-string
            // fallback over the raw header values.
            _ if legacy_raw == new_raw => (Vec::new(), false),
            _ => single(LocationMismatch {
                kind: LocationMismatchKind::Raw,
                param: None,
                // An unresolvable value cannot be parsed, so its query cannot
                // be selectively masked either; render neither side.
                legacy: Some(REDACTED.to_string()),
                new: Some(REDACTED.to_string()),
            }),
        },
    }
}

/// Part-wise comparison of two resolved `Location` URLs.
fn compare_targets(
    rules: &ResolvedLocationRules,
    limits: &DiffLimits,
    legacy: &Url,
    new: &Url,
) -> (Vec<LocationMismatch>, bool) {
    let mut mismatches = Bounded::new(limits);
    // `origin: ignore` exists for a legacy and a new service that intentionally
    // redirect to different hosts for the same logical destination.
    if rules.origin == OriginMode::Exact && origin_parts(legacy) != origin_parts(new) {
        mismatches.push(LocationMismatch {
            kind: LocationMismatchKind::Origin,
            param: None,
            legacy: Some(render_origin(legacy)),
            new: Some(render_origin(new)),
        });
    }
    if legacy.path() != new.path() {
        mismatches.push(LocationMismatch {
            kind: LocationMismatchKind::Path,
            param: None,
            legacy: Some(legacy.path().to_string()),
            new: Some(new.path().to_string()),
        });
    }
    let legacy_query = query_map(legacy, &rules.ignore_query_params);
    let new_query = query_map(new, &rules.ignore_query_params);
    for (param, l, n) in zip_by_key(&legacy_query, &new_query) {
        if l == n {
            continue;
        }
        mismatches.push(LocationMismatch {
            kind: LocationMismatchKind::Query,
            param: Some(param.clone()),
            legacy: render_query(param, l),
            new: render_query(param, n),
        });
    }
    mismatches.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(values: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in values {
            map.append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    fn cookie_rules() -> ResolvedSetCookieRules {
        ResolvedSetCookieRules {
            compare: true,
            ignore_cookies: Vec::new(),
            ignore_attributes: Vec::new(),
            compare_values: CookieValueMode::Exact,
        }
    }

    fn location_rules() -> ResolvedLocationRules {
        ResolvedLocationRules {
            compare: true,
            ignore_query_params: Vec::new(),
            origin: OriginMode::Exact,
        }
    }

    fn url(u: &str) -> Url {
        Url::parse(u).unwrap()
    }

    /// The cookie mismatches, asserting the default bound was not reached (the
    /// truncation case has its own test).
    fn cookies(
        rules: &ResolvedSetCookieRules,
        legacy: &HeaderMap,
        new: &HeaderMap,
    ) -> Vec<CookieMismatch> {
        let (mismatches, truncated) =
            compare_set_cookie(rules, &DiffLimits::default(), legacy, new);
        assert!(!truncated, "unexpected truncation");
        mismatches
    }

    /// The `Location` mismatches, asserting the default bound was not reached.
    fn locations(
        rules: &ResolvedLocationRules,
        legacy: &HeaderMap,
        legacy_url: Option<&Url>,
        new: &HeaderMap,
        new_url: Option<&Url>,
    ) -> Vec<LocationMismatch> {
        let (mismatches, truncated) = compare_location(
            rules,
            &DiffLimits::default(),
            legacy,
            legacy_url,
            new,
            new_url,
        );
        assert!(!truncated, "unexpected truncation");
        mismatches
    }

    #[test]
    fn identical_cookies_match() {
        let l = headers(&[
            ("set-cookie", "sid=abc; Path=/; HttpOnly; SameSite=Lax"),
            ("set-cookie", "theme=dark; Path=/"),
        ]);
        let n = l.clone();
        assert!(cookies(&cookie_rules(), &l, &n).is_empty());
    }

    #[test]
    fn attribute_names_are_case_insensitive_values_are_not() {
        let l = headers(&[("set-cookie", "sid=abc; httponly; samesite=Lax")]);
        let n = headers(&[("set-cookie", "sid=abc; HttpOnly; SameSite=Lax")]);
        assert!(cookies(&cookie_rules(), &l, &n).is_empty());

        let n = headers(&[("set-cookie", "sid=abc; HttpOnly; SameSite=lax")]);
        let mismatches = cookies(&cookie_rules(), &l, &n);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].kind, CookieMismatchKind::Attribute);
    }

    #[test]
    fn ignored_attributes_and_cookies_are_skipped() {
        let mut rules = cookie_rules();
        rules.ignore_attributes = vec!["expires".into()];
        rules.ignore_cookies = vec!["csrf".into()];
        let l = headers(&[
            (
                "set-cookie",
                "sid=abc; Expires=Wed, 21 Oct 2026 07:28:00 GMT",
            ),
            ("set-cookie", "csrf=only-legacy"),
        ]);
        let n = headers(&[(
            "set-cookie",
            "sid=abc; Expires=Thu, 22 Oct 2026 07:28:00 GMT",
        )]);
        assert!(cookies(&rules, &l, &n).is_empty());
    }

    #[test]
    fn presence_mode_compares_only_whether_a_value_exists() {
        let mut rules = cookie_rules();
        rules.compare_values = CookieValueMode::Presence;
        // Both sides set a value: the values themselves are not compared.
        let l = headers(&[("set-cookie", "sid=legacy-value; Path=/")]);
        let n = headers(&[("set-cookie", "sid=new-value; Path=/")]);
        assert!(cookies(&rules, &l, &n).is_empty());

        // Neither side sets a value: the cookie-deletion shape both sides emit
        // on logout is agreement, not a mismatch.
        let l = headers(&[("set-cookie", "sid=; Path=/; Max-Age=0")]);
        let n = headers(&[("set-cookie", "sid=; Path=/; Max-Age=0")]);
        assert!(cookies(&rules, &l, &n).is_empty());

        // Exactly one side sets a value: the sides disagree.
        let n = headers(&[("set-cookie", "sid=new-value; Path=/; Max-Age=0")]);
        let mismatches = cookies(&rules, &l, &n);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].kind, CookieMismatchKind::Value);
    }

    #[test]
    fn duplicate_names_pair_positionally() {
        let l = headers(&[("set-cookie", "flash=one"), ("set-cookie", "flash=two")]);
        let n = headers(&[("set-cookie", "flash=one"), ("set-cookie", "flash=two")]);
        assert!(cookies(&cookie_rules(), &l, &n).is_empty());
        // Reordered: positional pairing makes both slots differ.
        let n = headers(&[("set-cookie", "flash=two"), ("set-cookie", "flash=one")]);
        let mismatches = cookies(&cookie_rules(), &l, &n);
        assert_eq!(mismatches.len(), 2);
        assert!(mismatches
            .iter()
            .all(|m| m.kind == CookieMismatchKind::Value));
        // Extra copy on one side only.
        let n = headers(&[("set-cookie", "flash=one")]);
        let mismatches = cookies(&cookie_rules(), &l, &n);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].kind, CookieMismatchKind::Presence);
        assert_eq!(mismatches[0].new, None);
    }

    #[test]
    fn malformed_cookies_fall_back_to_exact_strings() {
        let l = headers(&[("set-cookie", "not-a-cookie")]);
        assert!(cookies(&cookie_rules(), &l, &l.clone()).is_empty());
        let n = headers(&[("set-cookie", "also-not-a-cookie")]);
        let mismatches = cookies(&cookie_rules(), &l, &n);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].kind, CookieMismatchKind::Malformed);
        assert_eq!(mismatches[0].name, MALFORMED_COOKIE);
    }

    #[test]
    fn compare_false_disables_the_dimension() {
        let mut rules = cookie_rules();
        rules.compare = false;
        let l = headers(&[("set-cookie", "sid=abc")]);
        let n = headers(&[("set-cookie", "other=xyz")]);
        assert!(cookies(&rules, &l, &n).is_empty());
    }

    #[test]
    fn relative_location_resolves_against_its_own_request_url() {
        let l = headers(&[("location", "/next?x=1")]);
        let n = headers(&[("location", "https://app.example/next?x=1")]);
        let base = url("https://app.example/start");
        assert!(locations(&location_rules(), &l, Some(&base), &n, Some(&base)).is_empty());
    }

    #[test]
    fn origin_ignore_tolerates_different_hosts() {
        let l = headers(&[("location", "https://legacy.example/dash")]);
        let n = headers(&[("location", "https://new.example/dash")]);
        let mismatches = locations(&location_rules(), &l, None, &n, None);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].kind, LocationMismatchKind::Origin);

        let mut rules = location_rules();
        rules.origin = OriginMode::Ignore;
        assert!(locations(&rules, &l, None, &n, None).is_empty());
    }

    #[test]
    fn ignored_query_params_drop_out_and_others_are_reported() {
        let mut rules = location_rules();
        rules.ignore_query_params = vec!["state".into()];
        let l = headers(&[("location", "https://app.example/cb?code=a&state=one")]);
        let n = headers(&[("location", "https://app.example/cb?code=a&state=two")]);
        assert!(locations(&rules, &l, None, &n, None).is_empty());

        let n = headers(&[("location", "https://app.example/cb?code=b&state=two")]);
        let mismatches = locations(&rules, &l, None, &n, None);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].kind, LocationMismatchKind::Query);
        assert_eq!(mismatches[0].param.as_deref(), Some("code"));
    }

    #[test]
    fn default_ports_are_part_of_the_origin_not_a_difference() {
        let l = headers(&[("location", "https://app.example:443/dash")]);
        let n = headers(&[("location", "https://app.example/dash")]);
        assert!(locations(&location_rules(), &l, None, &n, None).is_empty());
        // A non-default port IS a difference.
        let n = headers(&[("location", "https://app.example:8443/dash")]);
        let mismatches = locations(&location_rules(), &l, None, &n, None);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].kind, LocationMismatchKind::Origin);
    }

    #[test]
    fn non_http_schemes_compare_by_scheme_not_by_opaque_origin() {
        // `Url::origin()` returns a fresh opaque origin for every non-special
        // scheme, so comparing origins directly would make these mismatch.
        let l = headers(&[("location", "mailto:ops@example.com")]);
        let n = headers(&[("location", "mailto:ops@example.com")]);
        assert!(locations(&location_rules(), &l, None, &n, None).is_empty());
        // A different scheme is still an origin difference.
        let n = headers(&[("location", "sms:ops@example.com")]);
        let mismatches = locations(&location_rules(), &l, None, &n, None);
        assert_eq!(mismatches[0].kind, LocationMismatchKind::Origin);
        assert_eq!(mismatches[0].legacy.as_deref(), Some("mailto:"));
    }

    #[test]
    fn rendered_locations_never_include_userinfo() {
        let l = headers(&[("location", "https://alice:secret@app.example/next?t=1")]);
        let none = HeaderMap::new();
        let mismatches = locations(&location_rules(), &l, None, &none, None);
        assert_eq!(mismatches[0].kind, LocationMismatchKind::Presence);
        assert_eq!(
            mismatches[0].legacy.as_deref(),
            Some("https://app.example:443/next")
        );
        let rendered = serde_json::to_string(&mismatches).unwrap();
        assert!(!rendered.contains("alice"));
        assert!(!rendered.contains("secret"));
    }

    #[test]
    fn mismatch_lists_are_bounded_by_the_diff_limits() {
        let limits = DiffLimits {
            max_differences: 3,
            ..DiffLimits::default()
        };
        // A pathological response: many cookies, none of them matching.
        let mut l = HeaderMap::new();
        let mut n = HeaderMap::new();
        for index in 0..50 {
            l.append(
                "set-cookie",
                format!("c{index}=legacy-{index}").parse().unwrap(),
            );
            n.append(
                "set-cookie",
                format!("c{index}=new-{index}").parse().unwrap(),
            );
        }
        let (mismatches, truncated) = compare_set_cookie(&cookie_rules(), &limits, &l, &n);
        assert_eq!(mismatches.len(), 3);
        assert!(truncated);

        // The same cap applies to a Location with many differing query params.
        let query: String = (0..50)
            .map(|index| format!("p{index}={index}"))
            .collect::<Vec<_>>()
            .join("&");
        let l = headers(&[("location", &format!("https://app.example/cb?{query}"))]);
        let n = headers(&[("location", "https://app.example/cb")]);
        let (mismatches, truncated) =
            compare_location(&location_rules(), &limits, &l, None, &n, None);
        assert_eq!(mismatches.len(), 3);
        assert!(truncated);
    }

    #[test]
    fn query_parameter_order_does_not_matter() {
        let l = headers(&[("location", "https://app.example/cb?a=1&b=2")]);
        let n = headers(&[("location", "https://app.example/cb?b=2&a=1")]);
        assert!(locations(&location_rules(), &l, None, &n, None).is_empty());
    }

    #[test]
    fn sensitive_query_values_are_masked_in_the_diff() {
        let l = headers(&[(
            "location",
            "https://app.example/cb?access_token=legacy-secret",
        )]);
        let n = headers(&[("location", "https://app.example/cb?access_token=new-secret")]);
        let mismatches = locations(&location_rules(), &l, None, &n, None);
        let rendered = serde_json::to_string(&mismatches).unwrap();
        assert!(!rendered.contains("legacy-secret"));
        assert!(!rendered.contains("new-secret"));
    }

    #[test]
    fn location_presence_and_absence() {
        let none = HeaderMap::new();
        assert!(locations(&location_rules(), &none, None, &none, None).is_empty());
        let l = headers(&[("location", "https://app.example/x")]);
        let mismatches = locations(&location_rules(), &l, None, &none, None);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].kind, LocationMismatchKind::Presence);
        assert_eq!(mismatches[0].new, None);
    }

    #[test]
    fn unresolvable_location_falls_back_to_exact_strings() {
        // No request URL to resolve against: a relative value stays raw.
        let l = headers(&[("location", "/next")]);
        assert!(locations(&location_rules(), &l, None, &l.clone(), None).is_empty());
        let n = headers(&[("location", "/other")]);
        let mismatches = locations(&location_rules(), &l, None, &n, None);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].kind, LocationMismatchKind::Raw);
    }
}
