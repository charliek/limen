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
/// Shadow tasks in flight right now (dispatch through comparison).
pub const SHADOW_IN_FLIGHT: &str = "limen_shadow_in_flight";
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
/// Mismatch records *offered* to the diff sink's writer queue.
pub const DIFF_SINK_ENQUEUED_TOTAL: &str = "limen_diff_sink_enqueued_total";
/// Mismatch records appended to a daily sink file.
pub const DIFF_SINK_WRITTEN_TOTAL: &str = "limen_diff_sink_written_total";
/// Mismatch records that never reached the file, by reason.
pub const DIFF_SINK_DROPPED_TOTAL: &str = "limen_diff_sink_dropped_total";
/// Requests profiled by observe mode, by route.
pub const OBSERVE_OBSERVATIONS_TOTAL: &str = "limen_observe_observations_total";
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

/// The [`status_class`] bucket a successful response lands in.
///
/// Named rather than spelled inline because three sites must agree on it: this
/// module mints it, [`observe`](super::observe)'s recorder admits only this
/// class into the stability map, and the classifier's R8a asks whether the
/// class is absent. A literal in each place would let one be edited without the
/// others, and all three must mean the same thing.
pub const SUCCESS_STATUS_CLASS: &str = "2xx";

/// The status *class* label for a numeric status code: `2xx`, `4xx`, etc.
/// Bucketing keeps cardinality low (5 values, not 500).
pub fn status_class(status: u16) -> &'static str {
    match status / 100 {
        1 => "1xx",
        2 => SUCCESS_STATUS_CLASS,
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

/// A RAII guard that holds a gauge up for its lifetime: incremented when the
/// guard is taken, decremented on drop — including a panic unwind, so the gauge
/// can never be left leaked high.
///
/// Constructed only through the named helpers below, so the set of gauges that
/// can be held this way stays bounded and greppable.
pub struct GaugeGuard(&'static str);

impl GaugeGuard {
    fn enter(name: &'static str) -> Self {
        gauge!(name).increment(1.0);
        Self(name)
    }
}

impl Drop for GaugeGuard {
    fn drop(&mut self) {
        gauge!(self.0).decrement(1.0);
    }
}

/// Hold the in-flight *client request* gauge up until the guard is dropped.
pub fn in_flight() -> GaugeGuard {
    GaugeGuard::enter(IN_FLIGHT)
}

/// Hold the in-flight *shadow* gauge up until the guard is dropped.
///
/// Taken before the shadow task is spawned and moved into it, so the gauge
/// covers the whole fire-and-forget lifetime and every exit path — completion,
/// upstream failure, shadow timeout, or a panic unwinding the task — decrements
/// it exactly once. A leaked increment would leave a campaign verdict waiting
/// forever for a drain that already happened.
pub fn shadow_in_flight() -> GaugeGuard {
    GaugeGuard::enter(SHADOW_IN_FLIGHT)
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

/// Why a mismatch record never reached its daily sink file (a bounded, three-
/// value metric label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkDropReason {
    /// The writer queue was full — the writer could not keep up and the shadow
    /// task refused to block on it.
    QueueFull,
    /// The writer thread failed to open or write the daily file.
    IoError,
    /// The writer thread is gone (the channel is disconnected).
    WriterGone,
}

impl SinkDropReason {
    /// Every reason, so the label set can be registered up front
    /// ([`register_verdict_series`]).
    pub const ALL: [SinkDropReason; 3] = [
        SinkDropReason::QueueFull,
        SinkDropReason::IoError,
        SinkDropReason::WriterGone,
    ];

    /// A stable, lowercase label.
    pub fn as_str(self) -> &'static str {
        match self {
            SinkDropReason::QueueFull => "queue_full",
            SinkDropReason::IoError => "io_error",
            SinkDropReason::WriterGone => "writer_gone",
        }
    }
}

/// A mismatch record was *offered* to the diff sink's writer queue.
///
/// Counted at the offer, not at acceptance, so the drain equation
/// `enqueued == written + dropped` stays balanced when the queue refuses a
/// record: counting only accepted records would leave `enqueued` permanently
/// short and make a finished pipeline look like one still draining.
pub fn diff_sink_enqueued() {
    counter!(DIFF_SINK_ENQUEUED_TOTAL).increment(1);
}

/// A mismatch record was appended to its daily sink file.
pub fn diff_sink_written() {
    counter!(DIFF_SINK_WRITTEN_TOTAL).increment(1);
}

/// A mismatch record was dropped rather than persisted.
pub fn diff_sink_dropped(reason: SinkDropReason) {
    counter!(DIFF_SINK_DROPPED_TOTAL, "reason" => reason.as_str()).increment(1);
}

/// Touch every series limen's own typed gate tools read — a campaign verdict's
/// pipeline counters and the in-flight gauge `suggest-routes` quiesces against
/// — so all of them render from the very first scrape.
///
/// A verdict tool must be able to tell "nothing happened" from "this binary has
/// no such instrumentation", and lazily-registered metrics render those two
/// states identically: an absent series. Since verdict fails closed on a
/// missing input, an un-touched series would turn every clean, quiet run into a
/// tooling failure. Registering at zero makes the honest answer the one that
/// renders.
pub fn register_verdict_series() {
    counter!(DIFF_SINK_ENQUEUED_TOTAL).increment(0);
    counter!(DIFF_SINK_WRITTEN_TOTAL).increment(0);
    for reason in SinkDropReason::ALL {
        counter!(DIFF_SINK_DROPPED_TOTAL, "reason" => reason.as_str()).increment(0);
    }
    // An absolute set, not an increment: this runs once at startup, before any
    // shadow can have taken the guard.
    gauge!(SHADOW_IN_FLIGHT).set(0.0);
    // The client-request gauge is registered here for the same reason, on
    // behalf of a different tool: `limen suggest-routes` quiesces against
    // `limen_in_flight_requests` and refuses to read an absent series as zero,
    // so a proxy that has served no request yet must still render it. Same
    // startup-ordering argument — nothing can hold the guard this early.
    gauge!(IN_FLIGHT).set(0.0);
}

/// One request was observed by observe mode (after its sampling gate), so this
/// counts the same events the profile's `observations` field does.
///
/// The route id is the only label — the cardinality doctrine at the top of this
/// module applies unchanged, and observe mode's richer material (paths, query
/// names, content types) stays in the profile document where it is bounded per
/// route, never in a label.
pub fn observe_observation(route_id: &str) {
    counter!(OBSERVE_OBSERVATIONS_TOTAL, "route" => route_id.to_string()).increment(1);
}

/// Touch the observe counter for every configured route, so a profiled fleet
/// renders zeros rather than nothing.
///
/// Same reasoning as [`register_verdict_series`], applied per route: a lazily
/// registered counter makes "the observer saw nothing on this route" and "this
/// binary has no observe instrumentation" render identically, and the first is
/// a finding while the second is a broken deployment.
pub fn register_observe_series<'a>(route_ids: impl IntoIterator<Item = &'a str>) {
    for route_id in route_ids {
        counter!(OBSERVE_OBSERVATIONS_TOTAL, "route" => route_id.to_string()).increment(0);
    }
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
    use metrics_exporter_prometheus::PrometheusBuilder;

    /// The verdict series must render *at zero* after registration — not merely
    /// be spelled correctly somewhere. Asserted against a real rendered
    /// exposition (a local recorder keeps this test off the global one), so
    /// renderer drift breaks the test rather than the field.
    #[test]
    fn registration_renders_every_verdict_series_at_zero() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, register_verdict_series);
        let rendered = handle.render();

        for line in [
            "limen_diff_sink_enqueued_total 0",
            "limen_diff_sink_written_total 0",
            r#"limen_diff_sink_dropped_total{reason="queue_full"} 0"#,
            r#"limen_diff_sink_dropped_total{reason="io_error"} 0"#,
            r#"limen_diff_sink_dropped_total{reason="writer_gone"} 0"#,
            "limen_shadow_in_flight 0",
            "limen_in_flight_requests 0",
        ] {
            assert!(
                rendered.lines().any(|l| l == line),
                "expected `{line}` in the exposition:\n{rendered}"
            );
        }
    }

    /// The observe counter must render at zero for every configured route
    /// before any traffic — absence≠zero applied to the per-route series.
    #[test]
    fn registration_renders_the_observe_series_at_zero_per_route() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            register_observe_series(["alpha", "beta"]);
        });
        let rendered = handle.render();

        for line in [
            r#"limen_observe_observations_total{route="alpha"} 0"#,
            r#"limen_observe_observations_total{route="beta"} 0"#,
        ] {
            assert!(
                rendered.lines().any(|l| l == line),
                "expected `{line}` in the exposition:\n{rendered}"
            );
        }
    }

    #[test]
    fn sink_drop_reasons_are_the_documented_vocabulary() {
        assert_eq!(
            SinkDropReason::ALL.map(SinkDropReason::as_str),
            ["queue_full", "io_error", "writer_gone"]
        );
    }

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
