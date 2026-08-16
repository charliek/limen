//! Path templates — `/conversations/{id}` — and the overlap algebra the config
//! validator uses to prove a route table unambiguous (spec §5.2).
//!
//! A prefix says "everything under here"; a template says "exactly this shape".
//! The difference matters when one path under a prefix must be treated
//! differently from its siblings: `/conversations/export` is a report, every
//! other `/conversations/<id>` is a fetch, and no prefix can separate the two
//! without naming every id.
//!
//! Templates are matched **before** prefixes (see [`super::matcher`]), so a
//! template that half-overlaps a prefix route would quietly steal part of that
//! route's traffic. This module therefore carries not just a parser and a
//! matcher but the four predicates the validator needs to refuse such a table at
//! startup (safety invariant 7): [`co_matchable`], [`subsumes`],
//! [`intersects_prefix`], and [`contained_in_prefix`].

use std::fmt;

use thiserror::Error;

/// One segment of a parsed [`CompiledTemplate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// A literal segment, compared byte-for-byte against the raw request path.
    Literal(String),
    /// `{name}` — matches exactly one non-empty request segment. The name is
    /// documentation for the operator and a uniqueness key; matching itself
    /// never looks at it.
    Param(String),
}

/// Why a path template is rejected (spec §5.2). Every one of these is a
/// load-time error: a template that means something other than what its author
/// wrote would misroute traffic, and no shape here has a safe interpretation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TemplateParseError {
    /// The template did not start at the path root.
    #[error("must start with '/'")]
    MissingLeadingSlash,
    /// An empty segment: `//` inside the template, a trailing slash, or the
    /// bare `/`. None can ever match, because a matching path's segments are
    /// all non-empty.
    #[error(
        "segment {} is empty — `//`, a trailing slash, and `/` on its own are not templates",
        index + 1
    )]
    EmptySegment {
        /// Zero-based index of the empty segment.
        index: usize,
    },
    /// A `{` or `}` outside a segment that is exactly one parameter — `/v{n}`,
    /// `/{a}b`, `/{a}{b}`. A parameter spans a whole segment or nothing.
    #[error(
        "segment {segment:?} mixes literal text and a parameter — a parameter spans a whole \
         segment, as in `/v1/{{id}}`"
    )]
    MalformedSegment {
        /// The offending segment, as written.
        segment: String,
    },
    /// `{}` or a name that is not an identifier.
    #[error(
        "parameter name {name:?} is not an identifier — names must match [A-Za-z_][A-Za-z0-9_]*"
    )]
    BadParamName {
        /// The offending name, as written.
        name: String,
    },
    /// The same parameter name twice. Always a typo: the two positions are
    /// independent, so the repeat cannot be an equality constraint.
    #[error("duplicate parameter name {name:?}")]
    DuplicateParam {
        /// The repeated name.
        name: String,
    },
    /// Every segment is a parameter. Such a template matches *every* path of
    /// its length — a catch-all wearing a template's clothes, which the
    /// template tier would then evaluate ahead of every prefix route.
    #[error(
        "must carry at least one literal segment — an all-parameter template matches every path \
         of its length, and the template tier is consulted before every path_prefix route"
    )]
    NoLiteralSegment,
}

/// A path template compiled for matching and for overlap analysis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledTemplate {
    raw: String,
    segments: Vec<Segment>,
}

impl CompiledTemplate {
    /// Parse a template, or say precisely why it is not one.
    pub fn parse(raw: &str) -> Result<Self, TemplateParseError> {
        let rest = raw
            .strip_prefix('/')
            .ok_or(TemplateParseError::MissingLeadingSlash)?;
        let mut segments = Vec::new();
        for (index, part) in rest.split('/').enumerate() {
            if part.is_empty() {
                return Err(TemplateParseError::EmptySegment { index });
            }
            let segment = parse_segment(part)?;
            if let Segment::Param(name) = &segment {
                if segments
                    .iter()
                    .any(|s| matches!(s, Segment::Param(seen) if seen == name))
                {
                    return Err(TemplateParseError::DuplicateParam { name: name.clone() });
                }
            }
            segments.push(segment);
        }
        if !segments.iter().any(|s| matches!(s, Segment::Literal(_))) {
            return Err(TemplateParseError::NoLiteralSegment);
        }
        Ok(Self {
            raw: raw.to_string(),
            segments,
        })
    }

    /// The parsed segments, in order.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// The original textual form (e.g. `/conversations/{id}`).
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// How many segments are parameters — the specificity key the matcher sorts
    /// on, fewest first.
    pub fn param_count(&self) -> usize {
        self.segments
            .iter()
            .filter(|s| matches!(s, Segment::Param(_)))
            .count()
    }

    /// Whether this template matches a raw request path.
    ///
    /// Segment-count-exact, and deliberately **not** percent-decoded: the path
    /// is compared as it arrived, so a `%2F` inside a segment stays one
    /// character of that segment rather than splitting it. Decoding here would
    /// let an encoded slash smuggle a request into a shape the operator never
    /// wrote. A parameter matches any *non-empty* segment, so `//` and a
    /// trailing slash never match — those requests fall through to the prefix
    /// tier, which is where a path Limen cannot name belongs.
    pub fn matches_path(&self, path: &str) -> bool {
        let Some(rest) = path.strip_prefix('/') else {
            return false;
        };
        let mut actual = rest.split('/');
        for expected in &self.segments {
            let Some(part) = actual.next() else {
                return false;
            };
            if part.is_empty() {
                return false;
            }
            if let Segment::Literal(literal) = expected {
                if literal != part {
                    return false;
                }
            }
        }
        actual.next().is_none()
    }
}

impl fmt::Display for CompiledTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

fn parse_segment(part: &str) -> Result<Segment, TemplateParseError> {
    let malformed = || TemplateParseError::MalformedSegment {
        segment: part.to_string(),
    };
    if part.starts_with('{') && part.ends_with('}') && part.len() >= 2 {
        let name = &part[1..part.len() - 1];
        if name.contains(['{', '}']) {
            return Err(malformed());
        }
        if !is_param_name(name) {
            return Err(TemplateParseError::BadParamName {
                name: name.to_string(),
            });
        }
        return Ok(Segment::Param(name.to_string()));
    }
    // A brace anywhere else is an unterminated or partial parameter (`/v{n}`,
    // `/{id`). Treating it as literal text would silently route nothing.
    if part.contains(['{', '}']) {
        return Err(malformed());
    }
    Ok(Segment::Literal(part.to_string()))
}

fn is_param_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ---------------------------------------------------------------------------
// Overlap algebra (used by `crate::config::validate`)
// ---------------------------------------------------------------------------

/// Whether some request path matches **both** templates.
///
/// Different segment counts are disjoint by construction; otherwise the only
/// way to separate two templates is a position where both are literal and the
/// literals differ.
pub fn co_matchable(a: &CompiledTemplate, b: &CompiledTemplate) -> bool {
    a.segments.len() == b.segments.len()
        && a.segments.iter().zip(&b.segments).all(|pair| match pair {
            (Segment::Literal(x), Segment::Literal(y)) => x == y,
            // A parameter on either side can take whatever the other names.
            _ => true,
        })
}

/// Whether every path matching `narrow` also matches `wide` — i.e. `narrow` is
/// the more specific of the two, and the matcher's fewest-parameters-first
/// order puts it first. Identical shapes subsume both ways.
pub fn subsumes(narrow: &CompiledTemplate, wide: &CompiledTemplate) -> bool {
    narrow.segments.len() == wide.segments.len()
        && narrow
            .segments
            .iter()
            .zip(&wide.segments)
            .all(|pair| match pair {
                (_, Segment::Param(_)) => true,
                (Segment::Literal(x), Segment::Literal(y)) => x == y,
                (Segment::Param(_), Segment::Literal(_)) => false,
            })
}

/// A concrete path both templates match, for the error that refuses them. Only
/// meaningful when the two are [`co_matchable`]; where both sides are a
/// parameter the name stands in as the value, which is a legal segment.
pub fn witness_path(a: &CompiledTemplate, b: &CompiledTemplate) -> String {
    let mut out = String::new();
    for (x, y) in a.segments.iter().zip(&b.segments) {
        out.push('/');
        match (x, y) {
            (Segment::Literal(l), _) => out.push_str(l),
            (_, Segment::Literal(l)) => out.push_str(l),
            (Segment::Param(name), _) => out.push_str(name),
        }
    }
    out
}

/// `prefix` split at its segment boundaries: the complete segments it names,
/// plus a (possibly empty) partial trailing segment. `/a/b` → `(["a"], "b")`;
/// `/a/b/` → `(["a", "b"], "")`; `/` → `([], "")`. `None` if it is not a path
/// prefix at all — a shape validation has already rejected on its own.
fn split_prefix(prefix: &str) -> Option<(Vec<&str>, &str)> {
    let rest = prefix.strip_prefix('/')?;
    let mut parts: Vec<&str> = rest.split('/').collect();
    // `split` always yields at least one element, so this never underflows.
    let partial = parts.pop().unwrap_or("");
    Some((parts, partial))
}

/// Whether **some** path matching `t` starts with `prefix` — the question that
/// decides whether the two routes compete at all.
pub fn intersects_prefix(t: &CompiledTemplate, prefix: &str) -> bool {
    let Some((complete, partial)) = split_prefix(prefix) else {
        return false;
    };
    // A prefix naming as many complete segments as the template has cannot sit
    // in front of anything the template matches: those paths end at segment
    // `n`, leaving nothing for the prefix's boundary slash to precede.
    if complete.len() >= t.segments.len() {
        return false;
    }
    for (segment, part) in t.segments.iter().zip(complete.iter().copied()) {
        // `//` inside the prefix: a matching path never carries an empty
        // segment, so nothing can start with it.
        if part.is_empty() {
            return false;
        }
        if let Segment::Literal(literal) = segment {
            if literal != part {
                return false;
            }
        }
    }
    if partial.is_empty() {
        return true;
    }
    // The partial lands inside the template's next segment: a parameter can be
    // chosen to start with it, a literal must already do so.
    match &t.segments[complete.len()] {
        Segment::Param(_) => true,
        Segment::Literal(literal) => literal.starts_with(partial),
    }
}

/// Whether **every** path matching `t` starts with `prefix` — the template is a
/// refinement living entirely inside that prefix route's territory, which is
/// the one overlap with an unconditioned prefix that is safe to allow.
pub fn contained_in_prefix(t: &CompiledTemplate, prefix: &str) -> bool {
    let Some((complete, partial)) = split_prefix(prefix) else {
        return false;
    };
    if complete.len() >= t.segments.len() {
        return false;
    }
    // Every segment the prefix pins must be pinned by a literal in the template
    // too: a parameter there varies across the paths the template matches, so
    // some of them land outside the prefix. This needs no explicit `//` guard
    // the way `intersects_prefix` does — a parsed literal is never empty, so an
    // empty prefix segment already fails the equality below.
    for (segment, part) in t.segments.iter().zip(complete.iter().copied()) {
        match segment {
            Segment::Literal(literal) if literal == part => {}
            _ => return false,
        }
    }
    if partial.is_empty() {
        return true;
    }
    matches!(&t.segments[complete.len()], Segment::Literal(literal) if literal.starts_with(partial))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(raw: &str) -> CompiledTemplate {
        CompiledTemplate::parse(raw).unwrap_or_else(|e| panic!("{raw:?} should parse: {e}"))
    }

    fn err(raw: &str) -> TemplateParseError {
        CompiledTemplate::parse(raw).unwrap_err()
    }

    #[test]
    fn parses_literals_and_parameters() {
        let parsed = t("/conversations/{id}/messages");
        assert_eq!(
            parsed.segments(),
            [
                Segment::Literal("conversations".to_string()),
                Segment::Param("id".to_string()),
                Segment::Literal("messages".to_string()),
            ]
        );
        assert_eq!(parsed.param_count(), 1);
        assert_eq!(parsed.as_str(), "/conversations/{id}/messages");
        assert_eq!(t("/a/b").param_count(), 0);
    }

    #[test]
    fn a_template_must_start_at_the_root() {
        assert_eq!(
            err("conversations/{id}"),
            TemplateParseError::MissingLeadingSlash
        );
        assert_eq!(err(""), TemplateParseError::MissingLeadingSlash);
    }

    /// `/` alone, a trailing slash and `//` are the same mistake: a segment
    /// that no path can supply.
    #[test]
    fn empty_segments_are_rejected() {
        assert_eq!(err("/"), TemplateParseError::EmptySegment { index: 0 });
        assert_eq!(
            err("/conversations/{id}/"),
            TemplateParseError::EmptySegment { index: 2 }
        );
        assert_eq!(
            err("/conversations//{id}"),
            TemplateParseError::EmptySegment { index: 1 }
        );
    }

    #[test]
    fn a_parameter_must_span_a_whole_segment() {
        for raw in [
            "/v{n}/x",
            "/a/{id}x",
            "/a/x{id}",
            "/a/{x}{y}",
            "/a/{x",
            "/a/x}",
        ] {
            assert!(
                matches!(
                    CompiledTemplate::parse(raw),
                    Err(TemplateParseError::MalformedSegment { .. })
                ),
                "{raw:?} should be malformed, got {:?}",
                CompiledTemplate::parse(raw)
            );
        }
    }

    #[test]
    fn parameter_names_are_identifiers_and_unique() {
        assert_eq!(
            err("/a/{}"),
            TemplateParseError::BadParamName {
                name: String::new()
            }
        );
        for bad in ["1id", "id-x", "id.x", "id x"] {
            assert_eq!(
                err(&format!("/a/{{{bad}}}")),
                TemplateParseError::BadParamName {
                    name: bad.to_string()
                }
            );
        }
        assert_eq!(
            err("/a/{id}/b/{id}"),
            TemplateParseError::DuplicateParam {
                name: "id".to_string()
            }
        );
        // Underscores and digits after the first character are fine.
        assert_eq!(t("/a/{_id2}").param_count(), 1);
    }

    #[test]
    fn an_all_parameter_template_is_rejected() {
        assert_eq!(err("/{a}"), TemplateParseError::NoLiteralSegment);
        assert_eq!(err("/{a}/{b}"), TemplateParseError::NoLiteralSegment);
    }

    #[test]
    fn matching_is_segment_count_exact() {
        let parsed = t("/conversations/{id}");
        assert!(parsed.matches_path("/conversations/123"));
        assert!(!parsed.matches_path("/conversations"));
        assert!(!parsed.matches_path("/conversations/123/messages"));
        assert!(!parsed.matches_path("/other/123"));
        // Not a path at all.
        assert!(!parsed.matches_path("conversations/123"));
    }

    #[test]
    fn a_parameter_never_matches_an_empty_segment() {
        assert!(!t("/voices/{id}/preview").matches_path("/voices//preview"));
        assert!(!t("/conversations/{id}").matches_path("/conversations/"));
    }

    #[test]
    fn a_trailing_slash_does_not_match() {
        assert!(!t("/conversations/{id}").matches_path("/conversations/123/"));
        assert!(!t("/a/b").matches_path("/a/b/"));
    }

    /// No percent-decoding: an encoded slash is one character of one segment,
    /// never a segment boundary.
    #[test]
    fn matching_does_not_percent_decode() {
        let parsed = t("/conversations/{id}");
        assert!(parsed.matches_path("/conversations/a%2Fb"));
        assert!(!parsed.matches_path("/conversations/a%2Fb/c"));
        // The literal side is byte-equal too.
        assert!(!t("/a b/{id}").matches_path("/a%20b/1"));
    }

    #[test]
    fn co_matchability_needs_the_same_shape_and_no_clashing_literal() {
        assert!(co_matchable(&t("/a/{x}/c"), &t("/a/b/{y}")));
        assert!(co_matchable(&t("/a/{x}"), &t("/a/{y}")));
        // Different lengths are disjoint by construction.
        assert!(!co_matchable(&t("/a/{x}"), &t("/a/{x}/{y}")));
        // Clashing literals in the same position.
        assert!(!co_matchable(&t("/a/b"), &t("/a/c")));
    }

    #[test]
    fn subsumption_is_literal_refines_parameter() {
        assert!(subsumes(
            &t("/conversations/export"),
            &t("/conversations/{id}")
        ));
        assert!(!subsumes(
            &t("/conversations/{id}"),
            &t("/conversations/export")
        ));
        // Identical shapes subsume both ways, parameter names notwithstanding.
        assert!(subsumes(&t("/a/{x}"), &t("/a/{y}")));
        assert!(subsumes(&t("/a/{y}"), &t("/a/{x}")));
        // Neither direction: each pins a position the other leaves open.
        assert!(!subsumes(&t("/a/{x}/c"), &t("/a/b/{y}")));
        assert!(!subsumes(&t("/a/b/{y}"), &t("/a/{x}/c")));
    }

    #[test]
    fn a_witness_path_matches_both_templates() {
        for (a, b) in [
            ("/a/{x}/c", "/a/b/{y}"),
            ("/a/{x}/{y}", "/{t}/b/c"),
            ("/a/{x}", "/a/{y}"),
        ] {
            let (a, b) = (t(a), t(b));
            let path = witness_path(&a, &b);
            assert!(a.matches_path(&path), "{path} vs {a}");
            assert!(b.matches_path(&path), "{path} vs {b}");
        }
    }

    #[test]
    fn a_prefix_that_covers_whole_segments() {
        let parsed = t("/conversations/{id}");
        // The catch-all contains everything.
        assert!(intersects_prefix(&parsed, "/"));
        assert!(contained_in_prefix(&parsed, "/"));
        // A prefix ending exactly at a segment boundary.
        assert!(intersects_prefix(&parsed, "/conversations/"));
        assert!(contained_in_prefix(&parsed, "/conversations/"));
        // A different first segment does not meet it at all.
        assert!(!intersects_prefix(&parsed, "/voices/"));
        assert!(!contained_in_prefix(&parsed, "/voices/"));
    }

    #[test]
    fn a_prefix_that_ends_mid_segment() {
        let parsed = t("/voices/{id}");
        // Partial literal: every matching path still starts with it.
        assert!(intersects_prefix(&parsed, "/voi"));
        assert!(contained_in_prefix(&parsed, "/voi"));
        assert!(intersects_prefix(&parsed, "/voices"));
        assert!(contained_in_prefix(&parsed, "/voices"));
        // Partial against a parameter: some ids start with it, most do not.
        assert!(intersects_prefix(&parsed, "/voices/ex"));
        assert!(!contained_in_prefix(&parsed, "/voices/ex"));
        // A partial that no literal starts with.
        assert!(!intersects_prefix(&parsed, "/vox"));
    }

    #[test]
    fn a_prefix_longer_than_the_template_never_intersects() {
        let parsed = t("/a/b");
        // Exactly as many complete segments as the template has: `/a/b` cannot
        // start with `/a/b/`.
        assert!(!intersects_prefix(&parsed, "/a/b/"));
        assert!(!intersects_prefix(&parsed, "/a/b/c"));
        assert!(!contained_in_prefix(&parsed, "/a/b/"));
        // A prefix equal to the template's one path does intersect and contain.
        assert!(intersects_prefix(&parsed, "/a/b"));
        assert!(contained_in_prefix(&parsed, "/a/b"));
    }

    #[test]
    fn a_prefix_pinning_a_parameter_position_is_not_contained() {
        let parsed = t("/a/{x}/c");
        // The prefix covers the parameter segment completely.
        assert!(intersects_prefix(&parsed, "/a/b/"));
        assert!(!contained_in_prefix(&parsed, "/a/b/"));
        // …and partially.
        assert!(intersects_prefix(&parsed, "/a/b"));
        assert!(!contained_in_prefix(&parsed, "/a/b"));
    }

    #[test]
    fn an_empty_segment_in_the_prefix_intersects_nothing() {
        let parsed = t("/a/{x}");
        assert!(!intersects_prefix(&parsed, "//a"));
        assert!(!contained_in_prefix(&parsed, "//a"));
    }

    /// The two predicates must agree with brute force over a small path space:
    /// intersection means *some* generated path starts with the prefix,
    /// containment means *every* one does.
    #[test]
    fn the_prefix_predicates_agree_with_enumeration() {
        let templates = [
            "/a/{x}",
            "/a/{x}/c",
            "/a/b",
            "/{x}/b",
            "/voices/{id}/preview",
        ];
        let prefixes = [
            "/",
            "/a",
            "/a/",
            "/a/b",
            "/a/b/",
            "/a/b/c",
            "/ab",
            "/voices/",
            "/voices/1/p",
            "//a",
            "/a//",
        ];
        let values = ["a", "b", "c", "1", "bb"];
        for raw in templates {
            let parsed = t(raw);
            // Every path the template matches over a small value alphabet.
            let mut paths = vec![String::new()];
            for segment in parsed.segments() {
                let choices: Vec<&str> = match segment {
                    Segment::Literal(l) => vec![l.as_str()],
                    Segment::Param(_) => values.to_vec(),
                };
                let mut next = Vec::new();
                for head in &paths {
                    for choice in &choices {
                        next.push(format!("{head}/{choice}"));
                    }
                }
                paths = next;
            }
            for prefix in prefixes {
                let any = paths.iter().any(|p| p.starts_with(prefix));
                let all = paths.iter().all(|p| p.starts_with(prefix));
                // Enumeration is a lower bound on intersection: the alphabet is
                // finite, so `any` implies intersection but not the converse
                // (a prefix like `/a/z` intersects without a matching value).
                if any {
                    assert!(intersects_prefix(&parsed, prefix), "{raw} vs {prefix}");
                }
                assert_eq!(
                    contained_in_prefix(&parsed, prefix),
                    all,
                    "containment: {raw} vs {prefix}"
                );
            }
        }
    }
}
