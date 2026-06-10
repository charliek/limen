//! The comparison engine: normalization, hashing, diffing, and redaction.
//!
//! Comparison is hybrid (Section 7.1): normalize both responses, hash the
//! normalized form with `blake3`, and only generate a structural diff when the
//! hashes differ. All output surfaces are redacted before anything is logged.
//!
//! Submodules:
//! - [`jsonpath`] — the supported JSONPath subset and its parser (Phase 1).
//! - `normalize` — JSON normalization driven by merged contract rules (Phase 3).
//! - `hash` — `blake3` over the normalized representation (Phase 3).
//! - `diff` — bounded, redacted JSON-aware structural diff (Phase 3).
//! - `redact` — header / JSON-path / query redaction (Phase 3).
//! - `result` — `ComparisonResult` and `Mismatch` types (Phase 3).

pub mod jsonpath;
