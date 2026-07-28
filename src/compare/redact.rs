//! Redaction across every output surface (spec §7.5): JSON paths in diffs, and
//! header / query-parameter values in logs. No secret value may appear in any
//! diff or log.
//!
//! JSON-path redaction is driven by the contract's `redact_paths`. Because the
//! diff runs over real values (so a difference in a sensitive field is still
//! detected), redaction happens at *render* time: a difference whose location is
//! at or under a redacted path has its values replaced with [`REDACTED`].

use crate::compare::jsonpath::{JsonPath, Segment};

/// The placeholder shown in place of a redacted value.
pub const REDACTED: &str = "<redacted>";

/// Header names whose values are always redacted in logs and output. Lowercase.
///
/// This is the built-in default set. Per-deployment *configurable* header
/// redaction (spec §7.5) is a documented future addition — it needs a config
/// redaction block, which the MVP config model does not yet expose. Until then,
/// only the headers a route explicitly lists in `compare_headers` are ever
/// rendered, and these standard secret-bearing names are always masked.
pub const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "x-auth-token",
];

/// Query parameter names whose values are always redacted in logs. Lowercase.
///
/// `code` is here because an OAuth authorization code is a single-use
/// credential — it appears in redirect `Location` query strings, which the
/// `location` comparison dimension renders (spec §4.2).
pub const SENSITIVE_QUERY_PARAMS: &[&str] = &["access_token", "token", "api_key", "apikey", "code"];

/// One step of a concrete location within a JSON document, recorded as a diff
/// descends. Unlike a [`JsonPath`] pattern (which has wildcards), a location is
/// fully concrete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathStep {
    /// An object key.
    Key(String),
    /// An array index.
    Index(usize),
}

/// Render a concrete location as a JSONPath-like string, e.g. `$.items[0].id`.
pub fn render_path(steps: &[PathStep]) -> String {
    let mut out = String::from("$");
    for step in steps {
        match step {
            PathStep::Key(k) => {
                out.push('.');
                out.push_str(k);
            }
            PathStep::Index(i) => {
                out.push('[');
                out.push_str(&i.to_string());
                out.push(']');
            }
        }
    }
    out
}

/// Whether a single location step matches a pattern segment.
fn step_matches(step: &PathStep, seg: &Segment) -> bool {
    match (seg, step) {
        (Segment::Field(name), PathStep::Key(k)) => name == k,
        (Segment::Wildcard, PathStep::Index(_)) => true,
        _ => false,
    }
}

/// Whether a location and a redact pattern lie on the same branch — they agree
/// on every step they share. This is true when the location is **at or under**
/// the redacted path (location ⊇ pattern), when it is an **ancestor** of it
/// (location ⊂ pattern), and at the **root** (empty location, an ancestor of
/// everything). The ancestor/root cases are load-bearing: when a whole subtree
/// (up to the entire document, on a root type change) is added or removed, the
/// diff emits it at the ancestor's location, and that subtree may contain the
/// redacted descendant — so it must be masked too, or a secret leaks.
fn overlaps(location: &[PathStep], pattern: &[Segment]) -> bool {
    let shared = location.len().min(pattern.len());
    (0..shared).all(|i| step_matches(&location[i], &pattern[i]))
}

/// Whether a difference emitted at `location` could expose any redacted path.
pub fn is_redacted(location: &[PathStep], redact_paths: &[JsonPath]) -> bool {
    redact_paths
        .iter()
        .any(|p| overlaps(location, p.segments()))
}

/// The number of leading location steps safe to render for a **redacted**
/// location: the matched contract-defined prefix. Steps beyond it are derived
/// from the response data (e.g. an object keyed by a secret) and could leak, so
/// the rendered path is truncated to the shortest matching redact pattern.
pub fn safe_render_len(location: &[PathStep], redact_paths: &[JsonPath]) -> usize {
    redact_paths
        .iter()
        .filter(|p| overlaps(location, p.segments()))
        .map(|p| location.len().min(p.segments().len()))
        .min()
        .unwrap_or(location.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::jsonpath;

    fn parse(p: &str) -> JsonPath {
        jsonpath::parse(p).unwrap()
    }

    #[test]
    fn render_concrete_path() {
        let steps = vec![
            PathStep::Key("items".into()),
            PathStep::Index(0),
            PathStep::Key("id".into()),
        ];
        assert_eq!(render_path(&steps), "$.items[0].id");
    }

    #[test]
    fn exact_redact_path_matches() {
        let loc = vec![PathStep::Key("user".into()), PathStep::Key("email".into())];
        assert!(is_redacted(&loc, &[parse("$.user.email")]));
        assert!(!is_redacted(&loc, &[parse("$.user.name")]));
    }

    #[test]
    fn redacting_a_subtree_redacts_descendants() {
        let loc = vec![PathStep::Key("user".into()), PathStep::Key("email".into())];
        // Redacting `$.user` covers everything under it.
        assert!(is_redacted(&loc, &[parse("$.user")]));
    }

    #[test]
    fn wildcard_redact_matches_array_elements() {
        let loc = vec![
            PathStep::Key("items".into()),
            PathStep::Index(3),
            PathStep::Key("secret".into()),
        ];
        assert!(is_redacted(&loc, &[parse("$.items[*].secret")]));
        assert!(!is_redacted(&loc, &[parse("$.items[*].id")]));
    }

    #[test]
    fn ancestor_of_a_redact_path_is_redacted() {
        // A removed/added `$.user` subtree contains `$.user.email`, so emitting
        // it at the ancestor location must be masked.
        let user = vec![PathStep::Key("user".into())];
        assert!(is_redacted(&user, &[parse("$.user.email")]));
        // An unrelated ancestor is not redacted.
        let other = vec![PathStep::Key("other".into())];
        assert!(!is_redacted(&other, &[parse("$.user.email")]));
        // The document root (empty location) IS treated as a redacted ancestor:
        // a root-level diff emits the whole document, which could contain the
        // secret, so it must be masked.
        assert!(is_redacted(&[], &[parse("$.user.email")]));
        // The wildcard ancestor case: a removed `items[3]` element contains
        // `items[*].secret`.
        let elem = vec![PathStep::Key("items".into()), PathStep::Index(3)];
        assert!(is_redacted(&elem, &[parse("$.items[*].secret")]));
    }

    #[test]
    fn safe_render_len_truncates_to_the_contract_prefix() {
        // At/under: drop response-derived suffix (e.g. a secret object key).
        let under = vec![
            PathStep::Key("tokens".into()),
            PathStep::Key("secret-key".into()),
        ];
        assert_eq!(safe_render_len(&under, &[parse("$.tokens")]), 1);
        // Exact match: render the whole (contract-defined) path.
        let exact = vec![PathStep::Key("user".into()), PathStep::Key("email".into())];
        assert_eq!(safe_render_len(&exact, &[parse("$.user.email")]), 2);
        // Root: render nothing beyond `$`.
        assert_eq!(safe_render_len(&[], &[parse("$.user.email")]), 0);
    }
}
