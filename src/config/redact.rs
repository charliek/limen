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

/// Whether [`redact_url`] would drop something from `raw`: a userinfo, a query
/// or a fragment, or the whole value because it does not parse. For a caller
/// that keeps the URL as written when there is nothing to hide (a draft config
/// is read back, and a rewritten-but-equivalent URL is churn) and says so when
/// it had to cut.
pub(crate) fn carries_secrets(raw: &str) -> bool {
    match Url::parse(raw) {
        Ok(url) => {
            !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
        }
        Err(_) => true,
    }
}

/// A serde deserialization error with the configured values it quotes taken out.
///
/// serde builds its type errors out of the input it was handed: `invalid type:
/// string "redis://:pw@host", expected struct RedisFlagsConfig` is what a
/// nesting mistake in `flags.redis` prints, and an enum field says `unknown
/// variant `<the value>``. That is raised while the file is *parsed*, before any
/// validator can see the value, so it needs its own redaction. What stays is the
/// field path, the kind of thing found (`string`, `integer`), what was expected
/// and the line and column; a string, character or number found where something
/// else was expected, and an unknown variant name, become `(value redacted)`.
/// Field *names* (`unknown field`, `missing field`) are schema vocabulary and
/// are kept: they are how a typo'd key is found.
///
/// Anything unrecognized is passed through untouched, so a message serde has
/// not been taught to quote is no worse than before; a quote that never closes
/// is cut at the marker rather than guessed at.
pub(crate) fn serde_message(message: &str) -> String {
    // How to find what follows a quoted value, given the text after its opener.
    type After = fn(&str) -> &str;
    // (what serde prints before the value, what to keep of it, where it ends)
    const QUOTED: [(&str, &str, After); 5] = [
        ("string \"", "string", after_debug_string),
        ("character `", "character", after_backtick),
        ("integer `", "integer", after_backtick),
        ("floating point `", "floating point", after_backtick),
        ("unknown variant `", "unknown variant", after_variant_name),
    ];
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some((at, marker, kept, after)) = QUOTED
        .iter()
        .filter_map(|&(marker, kept, after)| rest.find(marker).map(|at| (at, marker, kept, after)))
        .min_by_key(|&(at, ..)| at)
    {
        out.push_str(&rest[..at]);
        out.push_str(kept);
        out.push_str(" (value redacted)");
        rest = after(&rest[at + marker.len()..]);
    }
    out.push_str(rest);
    out
}

/// What follows the closing backtick of a backtick-quoted number or character
/// whose opening backtick has been consumed; nothing when it never closes.
fn after_backtick(value: &str) -> &str {
    value.find('`').map_or("", |end| &value[end + 1..])
}

/// What follows an unknown variant's name, whose opening backtick has been
/// consumed: `unknown variant `<name>`, expected <the schema's variants>`. The
/// variants are fixed identifiers, so the *last* "`, expected " is the real
/// boundary even if the name itself contains one.
fn after_variant_name(value: &str) -> &str {
    value
        .rfind("`, expected ")
        .map_or("", |end| &value[end + 1..])
}

/// What follows the closing quote of a `{:?}`-rendered string whose opening
/// quote has been consumed, honoring `\\` and `\"`; nothing when it never closes.
fn after_debug_string(value: &str) -> &str {
    let mut chars = value.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '"' => return &value[i + 1..],
            _ => {}
        }
    }
    ""
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
    fn carries_secrets_is_true_exactly_when_redaction_cuts_something() {
        for hidden in [
            "https://u:p@legacy.internal",
            "https://u@legacy.internal",
            "https://legacy.internal/?t=1",
            "https://legacy.internal/#f",
            "https://legacy.internal:99999",
            "not a url",
        ] {
            assert!(carries_secrets(hidden), "{hidden}");
        }
        for plain in [
            "https://legacy.internal",
            "https://legacy.internal:8443/api/",
            "redis://localhost:6379/0",
        ] {
            assert!(!carries_secrets(plain), "{plain}");
        }
    }

    #[test]
    fn serde_message_keeps_the_path_and_what_was_expected() {
        assert_eq!(
            serde_message(
                "flags.redis: invalid type: string \"redis://:pw@host\", expected struct \
                 RedisFlagsConfig at line 9 column 12"
            ),
            "flags.redis: invalid type: string (value redacted), expected struct \
             RedisFlagsConfig at line 9 column 12"
        );
        assert_eq!(
            serde_message(
                "flags.provider: unknown variant `redis://:pw@h`, expected one of `static`, \
                 `file`, `redis` at line 3 column 13"
            ),
            "flags.provider: unknown variant (value redacted), expected one of `static`, \
             `file`, `redis` at line 3 column 13"
        );
    }

    #[test]
    fn serde_message_redacts_every_kind_of_quoted_value() {
        for (message, expected) in [
            (
                "invalid value: string \"a\", expected b",
                "invalid value: string (value redacted), expected b",
            ),
            (
                "invalid type: integer `5432`, expected a string",
                "invalid type: integer (value redacted), expected a string",
            ),
            (
                "invalid type: floating point `1.5`, expected a string",
                "invalid type: floating point (value redacted), expected a string",
            ),
            (
                "invalid type: character `x`, expected a string",
                "invalid type: character (value redacted), expected a string",
            ),
        ] {
            assert_eq!(serde_message(message), expected);
        }
    }

    #[test]
    fn serde_message_does_not_end_the_value_at_an_escaped_quote() {
        // serde renders the string with `{:?}`, so a quote inside it is `\"`.
        let shown = serde_message(&format!(
            "invalid type: string {:?}, expected a map at line 1 column 1",
            format!("a\"{CANARY}\\b\"{CANARY}")
        ));
        assert!(!shown.contains(CANARY), "{shown}");
        assert!(
            shown.ends_with(", expected a map at line 1 column 1"),
            "{shown}"
        );
    }

    #[test]
    fn serde_message_does_not_end_a_variant_at_a_quoted_expected() {
        let shown = serde_message(&format!(
            "unknown variant `x`, expected {CANARY}`, expected one of `a`, `b` at line 1"
        ));
        assert!(!shown.contains(CANARY), "{shown}");
        assert!(
            shown.ends_with(", expected one of `a`, `b` at line 1"),
            "{shown}"
        );
    }

    #[test]
    fn serde_message_cuts_a_quote_that_never_closes_and_leaves_the_rest_alone() {
        assert_eq!(
            serde_message(&format!("invalid type: string \"{CANARY}")),
            "invalid type: string (value redacted)"
        );
        // Field names are vocabulary, not values: they are how a typo is found.
        let unknown = "unknown field `rediss`, expected one of `provider`, `redis` at line 2";
        assert_eq!(serde_message(unknown), unknown);
        assert_eq!(serde_message("missing field `id`"), "missing field `id`");
        assert_eq!(
            serde_message("invalid type: map, expected a string"),
            "invalid type: map, expected a string"
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
