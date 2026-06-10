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
    /// Whether the difference list was truncated at the configured maximum.
    pub diff_truncated: bool,
    /// Compared headers that differed.
    pub header_mismatches: Vec<HeaderMismatch>,
}

impl ComparisonResult {
    /// Whether the responses are considered equivalent.
    pub fn is_match(&self) -> bool {
        self.status_match && self.body_match && self.header_mismatches.is_empty()
    }
}
