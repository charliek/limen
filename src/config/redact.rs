//! Rendering configured URLs without their secrets (safety invariant 5).
//!
//! A URL in the config can carry a password in its userinfo
//! (`redis://:pw@host`) and a token in its query or fragment. Every message
//! that names one — a validation error, `print-routes`, a startup failure — is
//! built here, so a mistyped or merely unusual value cannot put a credential in
//! a terminal or a CI log. The contract is the one the proxy already keeps for
//! upstream `Location` headers (`compare::headers::render_url`): origin and
//! path only, never userinfo, query or fragment.

use url::Url;

/// What a validator says about a configured URL it could not parse: the
/// parser's own reason and an explicit notice that the value is withheld.
///
/// `url::ParseError` renders one of a fixed set of phrases ("invalid port
/// number", "relative URL without a base", ...) and never any part of the
/// input, so the reason is safe to show and tells the operator which component
/// to look at. The input itself is not: it is exactly what failed to parse, so
/// there is no telling which part of it was the password.
pub(crate) fn unparseable_url(reason: url::ParseError) -> String {
    format!("is not a valid URL ({reason}; value redacted)")
}

/// A configured URL as `scheme://host[:port]/path`, for messages that need to
/// show *which* URL (the routing table `print-routes` prints). Userinfo, query
/// and fragment are dropped; an explicit default port is dropped too, as the
/// parser does. A bare root path is omitted, so `https://legacy.internal` reads
/// back as it was written. A URL with no host (`mailto:...`) shows its scheme
/// alone: its path is unstructured data, not a location.
///
/// Total, because a caller can't know the value was validated: what does not
/// parse renders as a fixed placeholder, never the raw text.
pub(crate) fn redact_url(raw: &str) -> String {
    let url = match Url::parse(raw) {
        Ok(url) => url,
        Err(reason) => return format!("<unparseable URL: {reason}; value redacted>"),
    };
    let Some(host) = url.host_str() else {
        return format!("{}:", url.scheme());
    };
    let port = url
        .port()
        .map_or_else(String::new, |port| format!(":{port}"));
    let path = if url.path() == "/" { "" } else { url.path() };
    format!("{}://{host}{port}{path}", url.scheme())
}

/// A Redis client-construction error without the value it rejected.
///
/// The `redis` crate appends the offending part of the URL to some of its
/// messages (`?protocol=<value>` comes back as `Invalid protocol version -
/// InvalidClientConfig: <value>`), and the URL is the credential-bearing config
/// value. The fixed description before the detail is what an operator needs, so
/// the detail alone is dropped; if the crate ever formats the detail anywhere
/// but the end, only the error's category is shown rather than guessing where
/// the value sits.
pub(crate) fn redis_client_error(error: &redis::RedisError) -> String {
    let shown = error.to_string();
    let Some(detail) = error.detail() else {
        return shown;
    };
    match shown.strip_suffix(detail) {
        Some(head) => format!("{} (value redacted)", head.trim_end_matches(": ")),
        None => format!("{} (details withheld)", error.category()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANARY: &str = "CANARY-9z";

    #[test]
    fn redact_url_keeps_origin_and_path_only() {
        for (raw, expected) in [
            ("https://legacy.internal", "https://legacy.internal"),
            ("https://legacy.internal/", "https://legacy.internal"),
            (
                "https://legacy.internal:8443/api/",
                "https://legacy.internal:8443/api/",
            ),
            ("http://[::1]:8080/a", "http://[::1]:8080/a"),
            // The default port is the parser's to drop, not ours to preserve.
            ("https://legacy.internal:443/a", "https://legacy.internal/a"),
        ] {
            assert_eq!(redact_url(raw), expected, "{raw}");
        }
    }

    #[test]
    fn redact_url_drops_userinfo_query_and_fragment() {
        for raw in [
            format!("https://user:{CANARY}@legacy.internal/a"),
            format!("https://{CANARY}@legacy.internal/a"),
            format!("https://legacy.internal/a?token={CANARY}"),
            format!("https://legacy.internal/a#{CANARY}"),
            format!("https://u:{CANARY}@legacy.internal:8443/a?token={CANARY}#{CANARY}"),
            format!("redis://:{CANARY}@legacy.internal:6379/0?protocol={CANARY}"),
        ] {
            let shown = redact_url(&raw);
            assert!(!shown.contains(CANARY), "{raw} rendered as {shown}");
            assert!(shown.contains("legacy.internal"), "{shown}");
        }
        assert_eq!(
            redact_url(&format!("https://u:{CANARY}@h:8443/a?t={CANARY}#f")),
            "https://h:8443/a"
        );
    }

    #[test]
    fn redact_url_never_echoes_what_does_not_parse() {
        for raw in [
            format!("redis://:{CANARY}@host:99999"),
            format!("{CANARY} is not a url"),
            format!("https://u:{CANARY}@[bad"),
        ] {
            let shown = redact_url(&raw);
            assert!(!shown.contains(CANARY), "{raw} rendered as {shown}");
            assert!(shown.contains("value redacted"), "{shown}");
        }
    }

    #[test]
    fn redact_url_shows_only_the_scheme_of_a_hostless_url() {
        assert_eq!(redact_url(&format!("mailto:{CANARY}@x.org")), "mailto:");
    }

    #[test]
    fn unparseable_url_names_the_reason_and_nothing_else() {
        let reason = Url::parse(&format!("redis://:{CANARY}@host:99999")).unwrap_err();
        assert_eq!(
            unparseable_url(reason),
            "is not a valid URL (invalid port number; value redacted)"
        );
    }

    #[test]
    fn redis_client_error_drops_the_detail_the_crate_appends() {
        let url = format!("redis://:{CANARY}@localhost:6379/0?protocol={CANARY}");
        let error = redis::Client::open(url.as_str()).unwrap_err();
        // Premise: the crate really does echo the query value, or this test
        // proves nothing about the redaction.
        assert!(error.to_string().contains(CANARY), "{error}");
        let shown = redis_client_error(&error);
        assert!(!shown.contains(CANARY), "{shown}");
        assert!(shown.contains("Invalid protocol version"), "{shown}");
    }

    #[test]
    fn redis_client_error_passes_a_detail_free_error_through() {
        let error = redis::Client::open("redis://localhost:6379/not-a-number").unwrap_err();
        assert_eq!(error.detail(), None);
        assert_eq!(redis_client_error(&error), error.to_string());
    }
}
