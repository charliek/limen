//! Observability: structured logging, Prometheus metrics, and request-id
//! propagation.
//!
//! Submodules grow as the build progresses:
//! - [`logging`] — `tracing` subscriber setup (available from Phase 0).
//! - `metrics` — metric definitions and registration (Phase 7).
//! - `request_id` — request/trace id extraction and propagation (Phase 7).

pub mod logging;
