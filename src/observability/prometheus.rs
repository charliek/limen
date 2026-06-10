//! Prometheus metrics: recorder installation, the metric vocabulary, and thin
//! emission helpers (spec §10.1).
//!
//! Metrics flow through the [`metrics`] facade so the call sites stay decoupled
//! from the exporter. [`install`] sets the global Prometheus recorder once and
//! returns a [`PrometheusHandle`] the control plane renders on `/metrics`.
//!
//! **Cardinality discipline (load-bearing):** every label here is bounded — a
//! route id (from config), an HTTP method, the upstream (`legacy`/`new`), a
//! status *class* (`2xx`…), or a small reason/result enum. Never label a metric
//! with a tenant/user/request id or a raw path; those go in logs, not labels.

use std::sync::OnceLock;

use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

use crate::observability::{ShadowFailure, SkipReason};
use crate::resilience::BreakerState;
use crate::routing::Upstream;

// --- Metric names ---------------------------------------------------------

/// Requests served, by route/method/upstream/status class.
pub const REQUESTS_TOTAL: &str = "limen_requests_total";
/// Client-facing request duration (time to response), by route/upstream.
pub const REQUEST_DURATION: &str = "limen_request_duration_seconds";
/// Upstream transport failures (connection error), by route/upstream.
pub const UPSTREAM_ERRORS_TOTAL: &str = "limen_upstream_errors_total";
/// Upstream timeouts, by route/upstream.
pub const UPSTREAM_TIMEOUTS_TOTAL: &str = "limen_upstream_timeouts_total";
/// In-flight client requests right now.
pub const IN_FLIGHT: &str = "limen_in_flight_requests";
/// Shadow requests dispatched to new, by route.
pub const SHADOW_TOTAL: &str = "limen_shadow_requests_total";
/// Shadows skipped, by route/reason.
pub const SHADOW_SKIPPED_TOTAL: &str = "limen_shadow_skipped_total";
/// Shadow requests that failed, by route/reason.
pub const SHADOW_FAILED_TOTAL: &str = "limen_shadow_failed_total";
/// Comparisons performed, by route/result (`match`/`mismatch`).
pub const COMPARISONS_TOTAL: &str = "limen_comparisons_total";
/// Comparisons skipped, by route/reason.
pub const COMPARISON_SKIPPED_TOTAL: &str = "limen_comparison_skipped_total";
/// Mismatches whose diff was sampled (and logged, redacted), by route.
pub const DIFF_SAMPLED_TOTAL: &str = "limen_diff_sampled_total";
/// Circuit-breaker state by route/upstream (0 closed, 1 half-open, 2 open).
pub const CIRCUIT_BREAKER_STATE: &str = "limen_circuit_breaker_state";
/// Whether the flag provider is stale (1) or fresh (0).
pub const FLAG_PROVIDER_STALE: &str = "limen_flag_provider_stale";
/// Age of the last successful flag refresh, in seconds.
pub const FLAG_STALENESS_SECONDS: &str = "limen_flag_provider_staleness_seconds";
/// Consecutive failed flag refreshes since the last success.
pub const FLAG_CONSECUTIVE_FAILURES: &str = "limen_flag_provider_consecutive_failures";

/// Latency histogram buckets in seconds (sub-millisecond to 10s).
const DURATION_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the global Prometheus recorder (idempotent) and return a handle for
/// rendering. Safe to call more than once; only the first call installs.
pub fn install() -> PrometheusHandle {
    HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .set_buckets_for_metric(
                    Matcher::Full(REQUEST_DURATION.to_string()),
                    DURATION_BUCKETS,
                )
                .expect("duration buckets are non-empty")
                .install_recorder()
                .expect("install Prometheus recorder")
        })
        .clone()
}

// --- Emission helpers -----------------------------------------------------

/// The status *class* label for a numeric status code: `2xx`, `4xx`, etc.
/// Bucketing keeps cardinality low (5 values, not 500).
pub fn status_class(status: u16) -> &'static str {
    match status / 100 {
        1 => "1xx",
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        5 => "5xx",
        _ => "other",
    }
}

/// Record a served request: increments the request counter and observes its
/// latency. `upstream` is the primary that served the client.
pub fn record_request(
    route_id: &str,
    method: &str,
    upstream: Upstream,
    status: u16,
    latency_s: f64,
) {
    counter!(
        REQUESTS_TOTAL,
        "route" => route_id.to_string(),
        "method" => method.to_string(),
        "upstream" => upstream.as_str(),
        "status_class" => status_class(status),
    )
    .increment(1);
    histogram!(
        REQUEST_DURATION,
        "route" => route_id.to_string(),
        "upstream" => upstream.as_str(),
    )
    .record(latency_s);
}

/// Record an upstream transport failure (connection error, no response).
pub fn record_upstream_error(route_id: &str, upstream: Upstream) {
    counter!(
        UPSTREAM_ERRORS_TOTAL,
        "route" => route_id.to_string(),
        "upstream" => upstream.as_str(),
    )
    .increment(1);
}

/// Record an upstream timeout.
pub fn record_upstream_timeout(route_id: &str, upstream: Upstream) {
    counter!(
        UPSTREAM_TIMEOUTS_TOTAL,
        "route" => route_id.to_string(),
        "upstream" => upstream.as_str(),
    )
    .increment(1);
}

/// A RAII guard that holds the in-flight request gauge up for its lifetime.
pub struct InFlight;

impl InFlight {
    /// Increment the in-flight gauge; the returned guard decrements on drop.
    pub fn enter() -> Self {
        gauge!(IN_FLIGHT).increment(1.0);
        Self
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        gauge!(IN_FLIGHT).decrement(1.0);
    }
}

/// A shadow request was dispatched to new.
pub fn shadow_dispatched(route_id: &str) {
    counter!(SHADOW_TOTAL, "route" => route_id.to_string()).increment(1);
}

/// A shadow request was skipped.
pub fn shadow_skipped(route_id: &str, reason: SkipReason) {
    counter!(SHADOW_SKIPPED_TOTAL, "route" => route_id.to_string(), "reason" => reason.as_str())
        .increment(1);
}

/// A shadow request failed.
pub fn shadow_failed(route_id: &str, failure: ShadowFailure) {
    counter!(SHADOW_FAILED_TOTAL, "route" => route_id.to_string(), "reason" => failure.as_str())
        .increment(1);
}

/// A comparison completed (`is_match` selects the `result` label).
pub fn comparison(route_id: &str, is_match: bool) {
    let result = if is_match { "match" } else { "mismatch" };
    counter!(COMPARISONS_TOTAL, "route" => route_id.to_string(), "result" => result).increment(1);
}

/// A comparison was skipped.
pub fn comparison_skipped(route_id: &str, reason: SkipReason) {
    counter!(COMPARISON_SKIPPED_TOTAL, "route" => route_id.to_string(), "reason" => reason.as_str())
        .increment(1);
}

/// A mismatch diff was sampled and logged (redacted).
pub fn diff_sampled(route_id: &str) {
    counter!(DIFF_SAMPLED_TOTAL, "route" => route_id.to_string()).increment(1);
}

/// Set the circuit-breaker state gauge for a route's new upstream.
pub fn set_breaker_state(route_id: &str, state: BreakerState) {
    let value = match state {
        BreakerState::Closed => 0.0,
        BreakerState::HalfOpen => 1.0,
        BreakerState::Open => 2.0,
    };
    gauge!(
        CIRCUIT_BREAKER_STATE,
        "route" => route_id.to_string(),
        "upstream" => Upstream::New.as_str(),
    )
    .set(value);
}

/// Set the flag-provider health gauges from a health snapshot.
pub fn set_flag_health(stale: bool, staleness_seconds: Option<f64>, consecutive_failures: u64) {
    gauge!(FLAG_PROVIDER_STALE).set(if stale { 1.0 } else { 0.0 });
    // Report a sentinel of -1 when there has never been a successful refresh.
    gauge!(FLAG_STALENESS_SECONDS).set(staleness_seconds.unwrap_or(-1.0));
    gauge!(FLAG_CONSECUTIVE_FAILURES).set(consecutive_failures as f64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_class_buckets() {
        assert_eq!(status_class(200), "2xx");
        assert_eq!(status_class(204), "2xx");
        assert_eq!(status_class(301), "3xx");
        assert_eq!(status_class(404), "4xx");
        assert_eq!(status_class(503), "5xx");
        assert_eq!(status_class(0), "other");
    }
}
