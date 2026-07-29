//! Comparison result types (spec §7.3).

use serde::Serialize;
use serde_json::Value;

/// Whether the body diff was structural JSON or an opaque byte comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    /// Both bodies were JSON; differences are structural.
    Json,
    /// At least one body was not JSON; only a body-level mismatch is recorded.
    NonJson,
}

/// How a single location changed between legacy and new.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// Present in both but with a different value.
    Changed,
    /// Present only in the new response.
    Added,
    /// Present only in the legacy response.
    Removed,
}

/// A single structural difference, bounded and redacted, ready to log.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Difference {
    /// JSONPath-like location, e.g. `$.device.name`.
    pub path: String,
    /// The kind of change.
    pub kind: ChangeKind,
    /// The legacy value (absent for `Added`), redacted/bounded for output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy: Option<Value>,
    /// The new value (absent for `Removed`), redacted/bounded for output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<Value>,
}

/// A compared header that differed between the two responses (values redacted
/// when the header name is sensitive).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HeaderMismatch {
    /// The header name.
    pub name: String,
    /// Legacy value (absent if the header was missing), possibly redacted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy: Option<String>,
    /// New value (absent if the header was missing), possibly redacted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
}

/// Which part of a `Set-Cookie` entry differed (spec §4.2, `set_cookie`).
///
/// The kinds are deliberately coarse and *value-free*: they say what disagreed,
/// never what the cookie was worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CookieMismatchKind {
    /// The cookie was set by exactly one side (or one side set more copies of
    /// the same name than the other).
    Presence,
    /// The cookie's value differed (`compare_values: exact`), or exactly one
    /// side's value was empty (`compare_values: presence`).
    Value,
    /// A cookie attribute differed, or was present on only one side.
    Attribute,
    /// An unparseable `Set-Cookie` entry differed under the exact-string
    /// fallback.
    Malformed,
}

/// A `Set-Cookie` difference between the two responses.
///
/// Rendered values are **never** raw cookie values: `Value` and `Malformed`
/// mismatches render [`crate::compare::redact::REDACTED`] (or `<empty>`), and
/// only attribute values — which carry no secret — are shown verbatim.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CookieMismatch {
    /// The cookie name, or [`MALFORMED_COOKIE`] for an unparseable entry.
    pub name: String,
    /// Which part of the cookie differed.
    pub kind: CookieMismatchKind,
    /// The attribute name, when `kind` is [`CookieMismatchKind::Attribute`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribute: Option<String>,
    /// The legacy side, rendered safely (absent when the legacy side had none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy: Option<String>,
    /// The new side, rendered safely (absent when the new side had none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
}

/// The placeholder name used for an unparseable `Set-Cookie` entry, which by
/// definition has no name to report.
pub const MALFORMED_COOKIE: &str = "<malformed>";

/// Which part of a `Location` URL differed (spec §4.2, `location`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocationMismatchKind {
    /// Exactly one side sent a `Location` header.
    Presence,
    /// Scheme, host, or port differed (`origin: exact` only).
    Origin,
    /// The URL path differed.
    Path,
    /// A query parameter differed (after `ignore_query_params` removal).
    Query,
    /// At least one side's `Location` could not be resolved to a URL, and the
    /// exact-string fallback found the sides different.
    Raw,
}

/// A `Location` difference between the two responses.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LocationMismatch {
    /// Which part of the URL differed.
    pub kind: LocationMismatchKind,
    /// The query parameter name, when `kind` is [`LocationMismatchKind::Query`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    /// The legacy side, rendered (absent when the legacy side had no such part).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy: Option<String>,
    /// The new side, rendered (absent when the new side had no such part).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
}

/// The outcome of comparing a legacy and a new response.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ComparisonResult {
    /// Whether the status codes matched (always true when status isn't compared).
    pub status_match: bool,
    /// Legacy status code.
    pub legacy_status: u16,
    /// New status code.
    pub new_status: u16,
    /// Whether the normalized bodies matched (hash-equal, or both non-JSON equal).
    pub body_match: bool,
    /// The diff kind, if a body diff was produced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_kind: Option<DiffKind>,
    /// Bounded, redacted structural differences.
    pub differences: Vec<Difference>,
    /// Whether any bounded list was truncated at `DiffLimits::max_differences`.
    /// The flag covers all three diff surfaces — body differences, cookie
    /// mismatches, and `Location` mismatches — each of which is capped
    /// independently so no single response can grow an unbounded log line.
    pub diff_truncated: bool,
    /// Compared headers that differed.
    pub header_mismatches: Vec<HeaderMismatch>,
    /// `Set-Cookie` differences (empty when the dimension is not compared).
    pub cookie_mismatches: Vec<CookieMismatch>,
    /// `Location` differences (empty when the dimension is not compared).
    pub location_mismatches: Vec<LocationMismatch>,
}

impl ComparisonResult {
    /// Whether the responses are considered equivalent.
    pub fn is_match(&self) -> bool {
        self.status_match
            && self.body_match
            && self.header_mismatches.is_empty()
            && self.cookie_mismatches.is_empty()
            && self.location_mismatches.is_empty()
    }

    /// The engine-neutral kinds of mismatch this result carries, sorted and
    /// de-duplicated.
    ///
    /// This is the vocabulary the cross-engine verdict table
    /// (`tests/lockstep/decisions.json`) records, so it must stay identical in
    /// Pharos: `status`, `body`, `header`, `set_cookie.<kind>`,
    /// `location.<kind>`. It is a *set*, deliberately order-independent, so the
    /// two engines need not agree on the order in which they discover
    /// mismatches — only on which ones exist.
    pub fn mismatch_kinds(&self) -> Vec<String> {
        let mut kinds: Vec<String> = Vec::new();
        if !self.status_match {
            kinds.push("status".to_string());
        }
        if !self.body_match {
            kinds.push("body".to_string());
        }
        if !self.header_mismatches.is_empty() {
            kinds.push("header".to_string());
        }
        for m in &self.cookie_mismatches {
            kinds.push(format!("set_cookie.{}", kind_name(&m.kind)));
        }
        for m in &self.location_mismatches {
            kinds.push(format!("location.{}", kind_name(&m.kind)));
        }
        kinds.sort();
        kinds.dedup();
        kinds
    }
}

/// The `snake_case` name of a mismatch kind, taken from its own `Serialize`
/// impl so the neutral vocabulary can never drift from the serialized shape.
fn kind_name<T: Serialize>(kind: &T) -> String {
    match serde_json::to_value(kind) {
        Ok(Value::String(name)) => name,
        // Unreachable: every mismatch kind is a unit variant, which serializes
        // to its `snake_case` name.
        _ => "unknown".to_string(),
    }
}
