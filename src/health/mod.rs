//! Health: the control-plane liveness and readiness endpoints.
//!
//! `/health/live` reports that the process is running; `/health/ready` reports
//! that config is valid and required providers are usable or in a safe fallback
//! mode, degrading rather than hard-failing when a provider is stale-but-safe
//! (Section 10.3).
//!
//! Submodules:
//! - `endpoints` — the `/health/live` and `/health/ready` handlers (Phase 2).
//! - `readiness` — readiness evaluation (Phase 2/7).
