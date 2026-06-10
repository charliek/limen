//! Resilience: circuit breaking, timeouts, and shadow concurrency limiting.
//!
//! These mechanisms enforce the fail-safe posture (Section 9): a per-route,
//! per-upstream circuit breaker routes around an unhealthy new upstream;
//! per-route timeouts bound primary and shadow calls; and a bounded shadow
//! concurrency limiter sheds shadow load rather than queuing it unboundedly.
//!
//! Submodules:
//! - [`circuit_breaker`] — the per-route breaker state machine (Phase 6).
//! - [`concurrency`] — shadow concurrency limiting (Phase 4).
//! - `timeouts` — primary / shadow timeouts are applied at the call sites in
//!   [`crate::http`] (per-route values), so there is no separate module.

pub mod circuit_breaker;
pub mod concurrency;

pub use circuit_breaker::{Admission, BreakerReservation, BreakerState, CircuitBreaker};
pub use concurrency::{ShadowLimiter, ShadowPermit};
