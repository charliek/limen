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

/// The backstop: `text` with every URL's secrets cut out, wherever it came from.
///
/// [`redact_url`] and the other helpers here make a *specific* message safe, and
/// make it read well. This is for everything that prints — every error and
/// diagnostic at the CLI's top-level output points goes through it — so a path
/// that interpolates a URL nobody thought to redact (a route id, a profile's
/// field, a control plane's reflection of its own request) is covered anyway.
///
/// Two lexical rules, neither of which needs the text to parse:
///
/// - A `scheme://…` run (up to whitespace, a quote or a bracket) keeps its
///   scheme, host, port and path. The userinfo is everything up to the last `@`
///   before the first `?` or `#` — the last, not the first, and not bounded by
///   the first `/`, so a password with an unescaped `/` or `@` (base64 has the
///   former) is still cut; a path that merely contains an `@` loses its host,
///   which is the right way round to be wrong. The query and fragment go.
/// - Anywhere else, a `?name=value&…` query or `#name=value` fragment has the
///   value of each `token`/`password`/`secret`/`key`/`auth`/… parameter masked.
///   That is the form a bare path takes (`prefix:/?token=…`).
///
/// A rewritten `://` run no longer parses as the URL it was, which is the point:
/// unlike [`redact_url`] it never needs the text to be a URL, so a run that
/// does not parse (`redis://:pw@host:99999`) is cut like any other, and a
/// literal like the docs' `scheme://host[:port]/path` passes through unchanged.
pub(crate) fn redact_urls_in_text(text: &str) -> String {
    let mut cut = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (head, after) = rest.split_at(at + 3);
        cut.push_str(head);
        let run_end = after.find(is_url_terminator).unwrap_or(after.len());
        // Sentence punctuation after a URL is not part of it.
        let run = after[..run_end].trim_end_matches([')', ']', '}', '.', ',', ';', ':', '!', '?']);
        let hierarchy = without_query(run);
        cut.push_str(
            hierarchy
                .rfind('@')
                .map_or(hierarchy, |at| &hierarchy[at + 1..]),
        );
        // Whatever ended the run is not the URL's: it continues as it was.
        rest = &after[run.len()..];
    }
    cut.push_str(rest);
    mask_secret_queries(&cut)
}

/// What ends a URL in running text: whitespace, a quote, an angle bracket, or
/// the backslash that escapes a quote in JSON.
fn is_url_terminator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '<' | '>' | '\\')
}

/// Mask the value of each secret-named parameter in every `?…` query and `#…`
/// fragment of `text` (see [`redact_urls_in_text`]).
fn mask_secret_queries(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(['?', '#']) {
        let (head, after) = rest.split_at(at + 1);
        out.push_str(head);
        let end = after.find(is_url_terminator).unwrap_or(after.len());
        let params: Vec<String> = after[..end]
            .split('&')
            .map(|pair| match pair.split_once('=') {
                // An empty value has nothing to hide. It is also what a second
                // pass finds in `?token=<redacted>`, where the `<` ends the run:
                // masking it would stack a second marker on the first.
                Some((name, value)) if !value.is_empty() && is_sensitive_name(name) => {
                    format!("{name}=<redacted>")
                }
                _ => pair.to_string(),
            })
            .collect();
        out.push_str(&params.join("&"));
        rest = &after[end..];
    }
    out.push_str(rest);
    out
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
///
/// Field *names* are another matter. `unknown field` and `missing field` are
/// schema vocabulary until the input supplies the name: an unknown key is
/// whatever the file said, and a URL pasted as a YAML key is quoted back
/// verbatim. So an unknown field's name is kept — it is how a typo'd key is
/// found — unless it is [`credential_shaped`].
///
/// Anything unrecognized is passed through untouched, so a message serde has
/// not been taught to quote is no worse than before. A marker with no closing
/// boundary was not written by serde (its messages always close), so it is text
/// from the input: it is left in place and scanning goes on after it, rather
/// than dropping the rest of the diagnostic. The one exception is a string
/// quote that never closes, cut at the marker: the remainder may be the value.
pub(crate) fn serde_message(message: &str) -> String {
    /// Splits the text after a marker into (the quoted name or value, what
    /// follows it), or `None` when nothing closes it.
    type Split = for<'a> fn(&'a str) -> Option<(&'a str, &'a str)>;
    #[derive(Clone, Copy, PartialEq)]
    enum Kind {
        /// Redacted; a marker that never closes stays in place.
        Value,
        /// Redacted; a quote that never closes is cut there.
        DebugString,
        /// Redacted only when [`credential_shaped`].
        Name,
    }
    // (what serde prints before it, what to keep of that, where it ends, what it is)
    const QUOTED: [(&str, &str, Split, Kind); 6] = [
        ("string \"", "string", split_debug_string, Kind::DebugString),
        ("character `", "character", split_backtick, Kind::Value),
        ("integer `", "integer", split_backtick, Kind::Value),
        (
            "floating point `",
            "floating point",
            split_backtick,
            Kind::Value,
        ),
        (
            "unknown variant `",
            "unknown variant",
            split_variant_name,
            Kind::Value,
        ),
        (
            "unknown field `",
            "unknown field",
            split_field_name,
            Kind::Name,
        ),
    ];
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some((at, marker, kept, split, kind)) = QUOTED
        .iter()
        .filter_map(|&(marker, kept, split, kind)| {
            rest.find(marker).map(|at| (at, marker, kept, split, kind))
        })
        .min_by_key(|&(at, ..)| at)
    {
        out.push_str(&rest[..at]);
        let after_marker = &rest[at + marker.len()..];
        match split(after_marker) {
            Some((value, after)) if kind == Kind::Name && !credential_shaped(value) => {
                out.push_str(marker);
                out.push_str(value);
                out.push('`');
                rest = after;
            }
            None if kind != Kind::DebugString => {
                out.push_str(marker);
                rest = after_marker;
            }
            found => {
                out.push_str(kept);
                out.push_str(if kind == Kind::Name {
                    " (name redacted)"
                } else {
                    " (value redacted)"
                });
                rest = found.map_or("", |(_, after)| after);
            }
        }
    }
    out.push_str(rest);
    out
}

/// The text before the closing backtick of a backtick-quoted number or
/// character, and what follows it.
fn split_backtick(text: &str) -> Option<(&str, &str)> {
    let end = text.find('`')?;
    Some((&text[..end], &text[end + 1..]))
}

/// An unknown variant's name and what follows it: `unknown variant `<name>`,
/// expected <the schema's variants>` or `, there are no variants`. The variants
/// are fixed identifiers, so the *last* boundary is the real one even if the
/// name itself contains one.
fn split_variant_name(text: &str) -> Option<(&str, &str)> {
    split_at_last(text, &["`, expected ", "`, there are no variants"])
}

/// An unknown field's name and what follows it, split the same way.
fn split_field_name(text: &str) -> Option<(&str, &str)> {
    split_at_last(text, &["`, expected ", "`, there are no fields"])
}

/// Split `text` at the last of `boundaries` (each beginning with the closing
/// backtick): the name before it, and what follows the backtick.
fn split_at_last<'a>(text: &'a str, boundaries: &[&str]) -> Option<(&'a str, &'a str)> {
    let end = boundaries.iter().filter_map(|b| text.rfind(b)).max()?;
    Some((&text[..end], &text[end + 1..]))
}

/// The text before the closing quote of a `{:?}`-rendered string whose opening
/// quote has been consumed, honoring `\\` and `\"`, and what follows it.
fn split_debug_string(text: &str) -> Option<(&str, &str)> {
    let mut chars = text.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '"' => return Some((&text[..i], &text[i + 1..])),
            _ => {}
        }
    }
    None
}

/// Whether a field name, an enum variant or a map key looks like a URL or a
/// credential rather than schema vocabulary: it names a host (`://`), carries
/// userinfo (`@`), is a query (`?`), or is a `token=…`-style assignment. A
/// typo'd `providr` or a legitimate `key_prefix` is neither, and stays visible.
fn credential_shaped(name: &str) -> bool {
    name.contains("://")
        || name.contains('@')
        || name.contains('?')
        || (name.contains('=') && is_sensitive_name(name))
}

/// Whether a query parameter or field name says it holds a secret.
fn is_sensitive_name(name: &str) -> bool {
    const SENSITIVE: [&str; 8] = [
        "token", "pass", "pwd", "secret", "key", "auth", "cred", "sig",
    ];
    let name = name.to_ascii_lowercase();
    SENSITIVE.iter().any(|word| name.contains(word))
}

/// A path-tracked serde error as an operator should read it: the same
/// `field.path: message` as `Display`, with every credential-shaped key (a URL
/// pasted as a YAML key, say) replaced wherever it appears, and the message put
/// through [`serde_message`].
///
/// The path is rendered from its segments rather than parsed back out of the
/// text, and the serde message carries its own copy of the path (`yaml_serde`
/// prefixes one), so a hidden key is replaced in the message as well.
pub(crate) fn serde_error<E: std::fmt::Display>(error: &serde_path_to_error::Error<E>) -> String {
    use serde_path_to_error::Segment;
    const HIDDEN: &str = "<redacted key>";

    let mut hidden = Vec::new();
    let mut path = String::new();
    let mut separator = "";
    for segment in error.path() {
        if !matches!(segment, Segment::Seq { .. }) {
            path.push_str(separator);
        }
        separator = ".";
        match segment {
            Segment::Seq { index } => path.push_str(&format!("[{index}]")),
            Segment::Map { key } | Segment::Enum { variant: key } => {
                if credential_shaped(key) {
                    hidden.push(key.as_str());
                    path.push_str(HIDDEN);
                } else {
                    path.push_str(key);
                }
            }
            Segment::Unknown => path.push('?'),
        }
    }
    let mut message = error.inner().to_string();
    for key in hidden {
        message = message.replace(key, HIDDEN);
    }
    let message = serde_message(&message);
    if error.path().iter().all(|s| matches!(s, Segment::Unknown)) {
        message
    } else {
        format!("{path}: {message}")
    }
}

/// `text` up to its first `?` or `#`. A matcher basis (`prefix:/devices`) never
/// has a query or fragment, so one a profile supplies with either is cut before
/// an error message quotes it; [`redact_urls_in_text`] cuts a URL's the same way.
pub(crate) fn without_query(text: &str) -> &str {
    text.find(['?', '#']).map_or(text, |end| &text[..end])
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
    fn redact_urls_in_text_cuts_userinfo_query_and_fragment_from_every_url() {
        for (text, expected) in [
            (
                format!("see https://user:{CANARY}@legacy.internal:8443/api/x?token={CANARY}#{CANARY} now"),
                "see https://legacy.internal:8443/api/x now",
            ),
            (
                format!("a redis://:{CANARY}@h:6379/0?protocol={CANARY} and http://u:{CANARY}@b/c"),
                "a redis://h:6379/0 and http://b/c",
            ),
            // Quoted, bracketed and JSON-escaped: the run ends where the text does.
            (
                format!(r#"got "https://u:{CANARY}@x/p?q={CANARY}" (and http://{CANARY}@y/z?k=v)."#),
                r#"got "https://x/p" (and http://y/z)."#,
            ),
            (
                format!(r#"{{"error": "bad https://u:{CANARY}@x/p?token={CANARY}\" again"}}"#),
                r#"{"error": "bad https://x/p\" again"}"#,
            ),
        ] {
            let shown = redact_urls_in_text(&text);
            assert_eq!(shown, expected, "{text}");
        }
    }

    /// What `redact_url` cannot do: a run that does not parse is still cut, and
    /// a password the URL grammar would never have accepted (base64 has `/`).
    #[test]
    fn redact_urls_in_text_cuts_what_does_not_parse() {
        for (text, expected) in [
            (
                format!("redis://:{CANARY}@host:99999"),
                "redis://host:99999",
            ),
            (
                format!("redis://:ab/{CANARY}+x==@host:6379/0"),
                "redis://host:6379/0",
            ),
            (format!("redis://:p@{CANARY}@host"), "redis://host"),
            (format!("https://u:{CANARY}@[bad"), "https://[bad"),
        ] {
            let shown = redact_urls_in_text(&text);
            assert_eq!(shown, expected, "{text}");
            assert!(!shown.contains(&format!(":{CANARY}")), "{shown}");
        }
    }

    /// The backstop is for everything that prints, so what has nothing to hide
    /// must come back byte for byte.
    #[test]
    fn redact_urls_in_text_leaves_the_rest_alone() {
        for text in [
            "",
            "no url here? maybe (older binary?) # a comment",
            "https://legacy.internal",
            "https://legacy.internal/",
            "http://[::1]:8080/a/b",
            "redis://localhost:6379/0",
            "a draft keeps only scheme://host[:port]/path, said so",
            "xmlns=\"http://www.w3.org/2000/svg\"",
            "/devices?page=2&sort=asc",
            "route \"r\" was profiled as \"prefix:/devices\"",
        ] {
            assert_eq!(redact_urls_in_text(text), text);
        }
    }

    /// Output that passes through twice — a report's text is cut where it is
    /// built and again where it is printed — must come out as it did the first
    /// time, in every form the first pass leaves behind.
    #[test]
    fn redact_urls_in_text_is_idempotent_in_every_form() {
        for (text, expected) in [
            // A `scheme://` run: the query is dropped outright.
            (
                format!("x https://u:{CANARY}@h/p?token={CANARY}#{CANARY}"),
                "x https://h/p",
            ),
            // A bare query, and the marker it is left with.
            (
                format!("prefix:/?token={CANARY}"),
                "prefix:/?token=<redacted>",
            ),
            (
                "/x?token=<redacted>&page=2".to_string(),
                "/x?token=<redacted>&page=2",
            ),
            (
                format!("/cb#access_token={CANARY}&expires_in=3600"),
                "/cb#access_token=<redacted>&expires_in=3600",
            ),
            // The marker as the HTML page holds it, escaped: the `&` of `&lt;`
            // leaves `token=` with an empty value for the second pass to find.
            (
                "<td>/x?token=&lt;redacted&gt;&amp;page=2</td>".to_string(),
                "<td>/x?token=&lt;redacted&gt;&amp;page=2</td>",
            ),
        ] {
            let once = redact_urls_in_text(&text);
            assert_eq!(once, expected, "{text}");
            assert_eq!(redact_urls_in_text(&once), once, "second pass of {text}");
        }
    }

    /// A query with no scheme in front of it: a matcher basis, a path.
    #[test]
    fn redact_urls_in_text_masks_secret_params_in_bare_queries() {
        for (text, expected) in [
            (
                format!("prefix:/?token={CANARY}"),
                "prefix:/?token=<redacted>",
            ),
            (
                format!("/a?page=2&api_key={CANARY}&sort=asc"),
                "/a?page=2&api_key=<redacted>&sort=asc",
            ),
            (
                format!("/cb#access_token={CANARY}&expires_in=3600"),
                "/cb#access_token=<redacted>&expires_in=3600",
            ),
            (
                format!(r#"{{"basis": "prefix:/?Password={CANARY}\"}}"#),
                r#"{"basis": "prefix:/?Password=<redacted>\"}"#,
            ),
        ] {
            assert_eq!(redact_urls_in_text(&text), expected, "{text}");
        }
    }

    /// The path-tracked error from parsing `yaml` as a `T` that it is not.
    fn yaml_error<T: serde::de::DeserializeOwned + std::fmt::Debug>(
        yaml: &str,
    ) -> serde_path_to_error::Error<yaml_serde::Error> {
        let de = yaml_serde::Deserializer::from_str(yaml);
        serde_path_to_error::deserialize::<_, T>(de).unwrap_err()
    }

    /// Parse `yaml` as a `Config` and return how the failure is reported.
    fn config_error(yaml: &str) -> String {
        serde_error(&yaml_error::<crate::config::model::Config>(yaml))
    }

    /// A map with arbitrary keys, to put a key where serde will quote it.
    #[derive(serde::Deserialize, Debug)]
    struct Aliases {
        #[allow(dead_code)]
        aliases: std::collections::BTreeMap<String, String>,
    }

    /// A URL used as a YAML key reaches serde's field path and its `unknown
    /// field` message, neither of which has a value to quote.
    #[test]
    fn a_credential_shaped_key_is_hidden_wherever_serde_puts_it() {
        for key in [
            format!("redis://:{CANARY}@host"),
            format!("{CANARY}@host"),
            format!("token={CANARY}"),
            format!("what?{CANARY}"),
            // A URL with nothing credential-shaped in it but its path: a webhook
            // path is a secret too, and `://` alone says "this is a URL".
            format!("https://hooks.example/T0/{CANARY}"),
        ] {
            let message = config_error(&format!("flags: {{redis: {{'{key}': null}}}}"));
            assert!(!message.contains(CANARY), "{key}: {message}");
            assert!(message.contains("flags.redis"), "the path stays: {message}");
            assert!(message.contains("unknown field"), "{message}");
            assert!(message.contains("expected one of"), "{message}");
        }
    }

    /// The key is in the message as well as the path: `yaml_serde` prefixes its
    /// own copy of the path to a type error, which is not an `unknown field`.
    #[test]
    fn a_credential_shaped_map_key_is_hidden_in_the_message_too() {
        let error = yaml_error::<Aliases>(&format!("aliases: {{'redis://:{CANARY}@h': [1]}}"));
        // Premise: the key is in serde's own message, not just in the path.
        assert!(error.inner().to_string().contains(CANARY), "{error}");

        let message = serde_error(&error);
        assert!(!message.contains(CANARY), "{message}");
        assert!(message.contains("expected a string"), "{message}");
        assert!(message.contains("<redacted key>"), "{message}");
    }

    #[test]
    fn serde_message_splits_a_name_at_the_last_boundary_of_either_kind() {
        assert_eq!(
            serde_message(&format!(
                "unknown variant `a`, there are no variants and {CANARY}`, expected one of `x`, `y` at line 1"
            )),
            "unknown variant (value redacted), expected one of `x`, `y` at line 1"
        );
    }

    #[test]
    fn an_ordinary_misspelt_key_stays_visible() {
        let message = config_error("flags: {redis: {urll: x, key_prefix: p}}");
        assert!(message.contains("flags.redis.urll"), "{message}");
        assert!(message.contains("unknown field `urll`"), "{message}");
    }

    /// The marker text as a map key: a variant name that never closes. The rest
    /// of the diagnostic — what was expected, where — must survive.
    #[test]
    fn a_marker_in_a_key_does_not_swallow_the_rest_of_the_diagnostic() {
        let message = serde_error(&yaml_error::<Aliases>(
            "aliases: {'unknown variant `': [1]}",
        ));
        assert!(message.contains("expected a string"), "{message}");
        assert!(message.contains("at line 1 column"), "{message}");
        assert!(message.contains("aliases."), "the path stays: {message}");
    }

    #[test]
    fn serde_message_keeps_the_remainder_when_a_marker_never_closes() {
        for text in [
            "unknown variant `x: invalid type: sequence, expected a string at line 3",
            "unknown field `x: invalid type: sequence, expected a string at line 3",
            "integer `5 then invalid type: map, expected a string at line 3",
        ] {
            assert_eq!(serde_message(text), text);
        }
    }

    #[test]
    fn serde_message_redacts_only_a_credential_shaped_unknown_field() {
        assert_eq!(
            serde_message("unknown field `urll`, expected one of `url`, `key_prefix` at line 2"),
            "unknown field `urll`, expected one of `url`, `key_prefix` at line 2"
        );
        assert_eq!(
            serde_message(&format!(
                "unknown field `redis://:{CANARY}@h`, expected one of `url`, `key_prefix` at line 2"
            )),
            "unknown field (name redacted), expected one of `url`, `key_prefix` at line 2"
        );
        assert_eq!(
            serde_message(&format!("unknown field `a@{CANARY}`, there are no fields")),
            "unknown field (name redacted), there are no fields"
        );
        assert_eq!(
            serde_message(&format!(
                "unknown variant `{CANARY}`, there are no variants"
            )),
            "unknown variant (value redacted), there are no variants"
        );
    }

    #[test]
    fn without_query_cuts_at_the_first_query_or_fragment() {
        assert_eq!(without_query("prefix:/devices"), "prefix:/devices");
        assert_eq!(without_query("prefix:/?token=x"), "prefix:/");
        assert_eq!(without_query("template:/a/{id}#f"), "template:/a/{id}");
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
