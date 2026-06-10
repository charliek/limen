//! Resilience: circuit breaking, timeouts, and shadow concurrency limiting.
//!
//! These mechanisms enforce the fail-safe posture (Section 9): a per-route,
//! per-upstream circuit breaker routes around an unhealthy new upstream;
//! per-route timeouts bound primary and shadow calls; and a bounded shadow
//! concurrency limiter sheds shadow load rather than queuing it unboundedly.
//!
//! Submodules:
//! - `circuit_breaker` — the per-route breaker state machine (Phase 6).
//! - `timeouts` — primary / shadow timeout helpers (Phase 6).
//! - `concurrency` — shadow concurrency limiting (Phase 4/6).
