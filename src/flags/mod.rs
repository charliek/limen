//! Feature flags: the provider trait and its implementations.
//!
//! Flags sit behind a trait so providers are swappable (Section 8). All
//! providers keep the *last known good* value on a failed refresh, track
//! staleness, and never crash the proxy; beyond `stale_ttl_ms` the configured
//! fail-safe mode applies.
//!
//! Submodules:
//! - `provider` — the `FlagProvider` trait and `FlagValue` (Phase 5).
//! - `static_provider`, `file_provider`, `redis_provider` — implementations.
//! - `health` — provider health and staleness tracking.
