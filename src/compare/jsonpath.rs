//! The supported JSONPath subset (spec §7.4) and its parser.
//!
//! To keep normalization fast and predictable — and identical to what Pharos
//! supports, so contracts are portable — Limen accepts only a documented subset
//! of JSONPath:
//!
//! - `$.field`
//! - `$.nested.field`
//! - `$.items[*].field` (wildcard over array elements)
//!
//! Anything outside this subset is a validation error at config/contract load
//! time. The subset may be expanded later, in lockstep across Limen and Pharos.
//!
//! Parsing produces a [`JsonPath`] of [`Segment`]s; the comparison engine
//! (Phase 3) walks those segments to apply ignore/redact/normalize transforms.

use std::fmt;

use thiserror::Error;

/// One step of a parsed [`JsonPath`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// `.field` — descend into the named object key.
    Field(String),
    /// `[*]` — fan out over every element of an array.
    Wildcard,
}

/// A JSONPath expression validated against the supported subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonPath {
    raw: String,
    segments: Vec<Segment>,
}

impl JsonPath {
    /// The parsed segments, in order, following the leading `$`.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// The original textual form (e.g. `$.items[*].id`).
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl fmt::Display for JsonPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// Why a JSONPath expression is rejected. Each variant carries the byte
/// position so callers can point at the offending character.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum JsonPathError {
    /// The expression did not start with the root `$`.
    #[error("path must start with `$`")]
    MissingRoot,
    /// A `.field` step had no name (e.g. `$.`, `$..a`, a trailing dot).
    #[error("empty field name at position {pos}")]
    EmptyField { pos: usize },
    /// A `[` was not the supported `[*]` wildcard (e.g. `[0]`, `['k']`).
    #[error("unsupported bracket at position {pos}: only `[*]` is allowed")]
    BadBracket { pos: usize },
    /// A character that cannot begin a step in the subset.
    #[error("unexpected character {ch:?} at position {pos}: expected `.` or `[*]`")]
    Unexpected { ch: char, pos: usize },
    /// The path selected no field (e.g. just `$`).
    #[error("path must reference at least one field (e.g. `$.field`)")]
    RootOnly,
    /// A `[*]` wildcard was not between two fields (leading, trailing, or
    /// adjacent to another wildcard). The supported form is `$.items[*].field`.
    #[error("`[*]` must sit between fields, as in `$.items[*].field`")]
    MisplacedWildcard,
    /// More than one `[*]` wildcard. The documented MVP subset allows at most
    /// one, so contracts stay portable to Pharos; the subset may be widened
    /// later in lockstep.
    #[error("at most one `[*]` wildcard is supported (the documented MVP subset)")]
    TooManyWildcards,
}

/// Characters allowed in an unquoted field name within the subset.
fn is_field_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
}

/// Parse and validate a JSONPath against the supported subset.
pub fn parse(input: &str) -> Result<JsonPath, JsonPathError> {
    let bytes: Vec<char> = input.chars().collect();
    let mut i = 0;

    match bytes.first() {
        Some('$') => i += 1,
        _ => return Err(JsonPathError::MissingRoot),
    }

    let mut segments = Vec::new();
    while i < bytes.len() {
        match bytes[i] {
            '.' => {
                i += 1;
                let start = i;
                while i < bytes.len() && is_field_char(bytes[i]) {
                    i += 1;
                }
                if i == start {
                    return Err(JsonPathError::EmptyField { pos: start });
                }
                segments.push(Segment::Field(bytes[start..i].iter().collect()));
            }
            '[' => {
                // The only supported bracket form is exactly `[*]`.
                if bytes.get(i + 1) == Some(&'*') && bytes.get(i + 2) == Some(&']') {
                    segments.push(Segment::Wildcard);
                    i += 3;
                } else {
                    return Err(JsonPathError::BadBracket { pos: i });
                }
            }
            ch => return Err(JsonPathError::Unexpected { ch, pos: i }),
        }
    }

    // Enforce the documented forms exactly: at least one field, and every `[*]`
    // wildcard must sit between fields (never first, last, or adjacent to
    // another wildcard). This keeps the subset identical to Pharos.
    if segments.is_empty() {
        return Err(JsonPathError::RootOnly);
    }
    if matches!(segments.first(), Some(Segment::Wildcard))
        || matches!(segments.last(), Some(Segment::Wildcard))
        || segments
            .windows(2)
            .any(|w| matches!(w, [Segment::Wildcard, Segment::Wildcard]))
    {
        return Err(JsonPathError::MisplacedWildcard);
    }
    if segments
        .iter()
        .filter(|s| matches!(s, Segment::Wildcard))
        .count()
        > 1
    {
        return Err(JsonPathError::TooManyWildcards);
    }

    Ok(JsonPath {
        raw: input.to_string(),
        segments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_field() {
        let p = parse("$.field").unwrap();
        assert_eq!(p.segments(), &[Segment::Field("field".into())]);
        assert_eq!(p.as_str(), "$.field");
    }

    #[test]
    fn parses_nested_field() {
        let p = parse("$.metadata.requestId").unwrap();
        assert_eq!(
            p.segments(),
            &[
                Segment::Field("metadata".into()),
                Segment::Field("requestId".into()),
            ]
        );
    }

    #[test]
    fn parses_wildcard_then_field() {
        let p = parse("$.items[*].id").unwrap();
        assert_eq!(
            p.segments(),
            &[
                Segment::Field("items".into()),
                Segment::Wildcard,
                Segment::Field("id".into()),
            ]
        );
    }

    #[test]
    fn rejects_more_than_one_wildcard() {
        // The documented MVP subset allows a single wildcard; nested wildcards
        // stay out until the subset is widened in lockstep with Pharos.
        assert_eq!(parse("$.a[*].b[*].c"), Err(JsonPathError::TooManyWildcards));
    }

    #[test]
    fn rejects_root_only() {
        assert_eq!(parse("$"), Err(JsonPathError::RootOnly));
    }

    #[test]
    fn rejects_misplaced_wildcards() {
        // Trailing, leading, and adjacent wildcards are all outside the subset.
        assert_eq!(parse("$.devices[*]"), Err(JsonPathError::MisplacedWildcard));
        assert_eq!(parse("$[*].a"), Err(JsonPathError::MisplacedWildcard));
        assert_eq!(parse("$.a[*][*].b"), Err(JsonPathError::MisplacedWildcard));
    }

    #[test]
    fn allows_hyphen_and_underscore_and_digits() {
        let p = parse("$.x-request-id.part_2.v3").unwrap();
        assert_eq!(
            p.segments(),
            &[
                Segment::Field("x-request-id".into()),
                Segment::Field("part_2".into()),
                Segment::Field("v3".into()),
            ]
        );
    }

    #[test]
    fn rejects_missing_root() {
        assert_eq!(parse("field"), Err(JsonPathError::MissingRoot));
        assert_eq!(parse(".field"), Err(JsonPathError::MissingRoot));
        assert_eq!(parse(""), Err(JsonPathError::MissingRoot));
    }

    #[test]
    fn rejects_recursive_descent() {
        // `$..field` -> after the first dot, the next char is another dot, so
        // the field name is empty.
        assert_eq!(parse("$..field"), Err(JsonPathError::EmptyField { pos: 2 }));
    }

    #[test]
    fn rejects_trailing_dot() {
        assert_eq!(parse("$.a."), Err(JsonPathError::EmptyField { pos: 4 }));
    }

    #[test]
    fn rejects_array_index() {
        assert_eq!(
            parse("$.items[0]"),
            Err(JsonPathError::BadBracket { pos: 7 })
        );
    }

    #[test]
    fn rejects_bracket_quote_notation() {
        assert_eq!(
            parse("$['field']"),
            Err(JsonPathError::BadBracket { pos: 1 })
        );
    }

    #[test]
    fn rejects_unterminated_bracket() {
        assert_eq!(parse("$.a[*"), Err(JsonPathError::BadBracket { pos: 3 }));
        assert_eq!(parse("$.a["), Err(JsonPathError::BadBracket { pos: 3 }));
    }

    #[test]
    fn rejects_unexpected_char() {
        assert_eq!(
            parse("$field"),
            Err(JsonPathError::Unexpected { ch: 'f', pos: 1 })
        );
    }
}
