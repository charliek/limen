//! Observability: structured logging, Prometheus metrics, and request-id
//! propagation.
//!
//! Submodules:
//! - [`logging`] — `tracing` subscriber setup (text or JSON).
//! - [`metrics`] — the shadow/comparison observer (records via the facade).
//! - [`observe`] — observe mode: passive per-route traffic profiling.
//! - [`prometheus`] — the Prometheus recorder, metric vocabulary, and emission
//!   helpers rendered on the control-plane `/metrics` endpoint.
//! - [`request_id`] — request/trace id extraction and propagation.
//! - [`sink`] — the optional durable mismatch sink (JSONL) and the `limen
//!   report` aggregation over it.

pub mod logging;
pub mod metrics;
pub mod observe;
pub mod prometheus;
pub mod request_id;
pub mod sink;

pub use metrics::{Fanout, MetricsObserver, ShadowFailure, ShadowMeta, ShadowObserver, SkipReason};
pub use observe::{Observation, ObserveProfile, ObserveRecorder, ResponseOrigin, RouteProfile};
pub use sink::SinkObserver;
