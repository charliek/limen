//! Observability: structured logging, Prometheus metrics, and request-id
//! propagation.
//!
//! Submodules:
//! - [`logging`] — `tracing` subscriber setup (text or JSON).
//! - [`metrics`] — the shadow/comparison observer (records via the facade).
//! - [`prometheus`] — the Prometheus recorder, metric vocabulary, and emission
//!   helpers rendered on the control-plane `/metrics` endpoint.
//! - [`request_id`] — request/trace id extraction and propagation.

pub mod logging;
pub mod metrics;
pub mod prometheus;
pub mod request_id;

pub use metrics::{MetricsObserver, ShadowFailure, ShadowMeta, ShadowObserver, SkipReason};
