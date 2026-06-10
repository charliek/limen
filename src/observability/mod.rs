//! Observability: structured logging, Prometheus metrics, and request-id
//! propagation.
//!
//! Submodules grow as the build progresses:
//! - [`logging`] — `tracing` subscriber setup (available from Phase 0).
//! - [`metrics`] — shadow/comparison stats + observer (Phase 4); the full
//!   Prometheus metric set and `/metrics` endpoint land in Phase 7.
//! - `request_id` — request/trace id extraction and propagation (Phase 7).

pub mod logging;
pub mod metrics;

pub use metrics::{MetricsObserver, ShadowFailure, ShadowObserver, SkipReason, Stats};
