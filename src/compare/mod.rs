//! The comparison engine: normalization, hashing, diffing, and redaction.
//!
//! Comparison is hybrid (Section 7.1): normalize both responses, hash the
//! normalized form with `blake3`, and only generate a structural diff when the
//! hashes differ. All output surfaces are redacted before anything is logged.
//!
//! Submodules (introduced in Phase 3):
//! - `normalize` — JSON normalization driven by merged contract rules.
//! - `jsonpath` — the supported JSONPath subset.
//! - `hash` — `blake3` over the normalized representation.
//! - `diff` — bounded, redacted JSON-aware structural diff.
//! - `redact` — header / JSON-path / query redaction.
//! - `result` — `ComparisonResult` and `Mismatch` types.
