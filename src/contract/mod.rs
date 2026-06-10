//! The shared behavioral contract: model, loading, and merge.
//!
//! The contract is the single source of truth for *comparison semantics* —
//! what to compare and how to normalize it — shared, unchanged, with the Pharos
//! functional test suite (Section 4). Limen loads it once at startup and merges
//! the behavioral rules onto each route's operational comparison policy.
//!
//! Submodules (introduced in Phase 1):
//! - `model` — serde structs for the contract (YAML/JSON).
//! - `load` — file loading and `path#routeId` reference resolution.
//! - `merge` — merging contract rules with route operational config.
