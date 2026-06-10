//! Operational configuration: the model, layered loading, and semantic
//! validation.
//!
//! Configuration is layered with later sources overriding earlier ones
//! (Section 5.1): built-in defaults < config file < environment < CLI.
//!
//! Submodules (introduced in Phase 1):
//! - `model` — serde structs for `limen.config.yaml`.
//! - `load` — layered loading.
//! - `validate` — semantic validation (URLs, percentages, timeouts, refs).
