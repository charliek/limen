//! The shadow/comparison observer (spec §7.3, §10.1).
//!
//! [`ShadowObserver`] receives shadow and comparison outcomes off the client
//! path. The production [`MetricsObserver`] records them as Prometheus metrics
//! (via [`crate::observability::prometheus`]) and emits the redacted mismatch
//! log. The trait indirection keeps the shadow path testable: a test supplies a
//! capturing observer to assert on outcomes without scraping metrics or logs.

use std::sync::Arc;

use axum::http::Method;
use tracing::{debug, warn};

use crate::compare::result::ComparisonResult;
use crate::observability::prometheus;

/// Why a shadow or comparison was skipped (low-cardinality metric labels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The shadow concurrency limit was saturated.
    ConcurrencyLimit,
    /// A response body exceeded `max_body_bytes`.
    ResponseTooLarge,
    /// A request body on a write-shadowing route exceeded `max_body_bytes`, so
    /// it could not be buffered for replay (spec §6.1); the primary still
    /// received it, streamed and unchanged.
    RequestTooLarge,
    /// The primary response was a `text/event-stream`, which by design never
    /// completes — it is skipped before a byte is buffered so the client keeps
    /// the streaming path.
    EventStream,
    /// Buffering the primary response for comparison ran out of the request's
    /// `primary_ms` budget (a trickling or stalled body); the client is served
    /// the streamed body instead of waiting on a comparison that may never
    /// become possible.
    ResponseBufferTimeout,
}

impl SkipReason {
    /// Every reason, so the label set can be registered up front
    /// ([`crate::observability::prometheus::register_skip_series`]). A verdict
    /// gates on these counters, so it must be able to read a zero rather than
    /// an absence — and it can only do that if every reason is pre-touched.
    pub const ALL: [SkipReason; 5] = [
        SkipReason::ConcurrencyLimit,
        SkipReason::ResponseTooLarge,
        SkipReason::RequestTooLarge,
        SkipReason::EventStream,
        SkipReason::ResponseBufferTimeout,
    ];

    /// A stable, lowercase label.
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::ConcurrencyLimit => "concurrency_limit",
            SkipReason::ResponseTooLarge => "response_too_large",
            SkipReason::RequestTooLarge => "request_too_large",
            SkipReason::EventStream => "event_stream",
            SkipReason::ResponseBufferTimeout => "response_buffer_timeout",
        }
    }
}

/// How a shadow request to the new upstream failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowFailure {
    /// The new upstream did not respond before the shadow timeout.
    Timeout,
    /// The shadow request errored (connection failure, etc.).
    Error,
}

impl ShadowFailure {
    /// Every failure mode, for the same pre-registration reason as
    /// [`SkipReason::ALL`].
    pub const ALL: [ShadowFailure; 2] = [ShadowFailure::Timeout, ShadowFailure::Error];

    /// A stable, lowercase label.
    pub fn as_str(self) -> &'static str {
        match self {
            ShadowFailure::Timeout => "timeout",
            ShadowFailure::Error => "error",
        }
    }
}

/// Request-scoped identifiers passed to every [`ShadowObserver`] callback
/// (spec §7.3, §10.1). Built once per shadowed request (see
/// [`crate::http::shadow::ShadowRequest::meta`]) so observers never re-derive
/// them (e.g. re-parsing `x-request-id` out of headers).
///
/// Only `route_id` is ever used as a Prometheus label — `request_id`, `method`,
/// and `path` are for structured logs and (a later sink) diff records, and must
/// never be turned into metric labels (cardinality discipline, spec §10.1).
#[derive(Debug, Clone)]
pub struct ShadowMeta {
    /// The matched route id (low-cardinality; the only field used as a metric
    /// label).
    pub route_id: String,
    /// The originating request's resolved `x-request-id` (client-supplied or
    /// generated — see [`crate::observability::request_id::resolve`]).
    pub request_id: String,
    /// The request method — `GET`/`HEAD` reads, plus any write method a route
    /// opts into via `comparison.shadow_methods` (see `SHADOWABLE_WRITE_METHODS`
    /// for the eligible verbs, spec §6.1).
    pub method: Method,
    /// The concrete request path (not a path template) — fine for logs/sink,
    /// never a metric label.
    pub path: String,
}

/// Receives shadow and comparison outcomes off the client path.
pub trait ShadowObserver: Send + Sync {
    /// A shadow request was dispatched to the new upstream.
    fn shadow_dispatched(&self, meta: &ShadowMeta);
    /// A comparison completed (match or mismatch).
    fn comparison(&self, meta: &ShadowMeta, result: &ComparisonResult);
    /// A shadow request was not dispatched.
    fn shadow_skipped(&self, meta: &ShadowMeta, reason: SkipReason);
    /// A shadow request was dispatched but failed.
    fn shadow_failed(&self, meta: &ShadowMeta, failure: ShadowFailure);
    /// A comparison was not performed (e.g. a response was too large to buffer).
    fn comparison_skipped(&self, meta: &ShadowMeta, reason: SkipReason);
}

/// Delivers every callback to several observers in order.
///
/// The seam that lets an optional surface (today: the
/// [diff sink][crate::observability::sink::SinkObserver]) be added *alongside*
/// [`MetricsObserver`] rather than in place of it — metrics and logs keep
/// working exactly as before whether or not the sink is configured.
pub struct Fanout {
    observers: Vec<Arc<dyn ShadowObserver>>,
}

impl Fanout {
    /// Fan out to `observers`, in the given order.
    pub fn new(observers: Vec<Arc<dyn ShadowObserver>>) -> Self {
        Self { observers }
    }

    /// Invoke one callback on every observer, in order.
    fn each(&self, callback: impl Fn(&dyn ShadowObserver)) {
        for observer in &self.observers {
            callback(observer.as_ref());
        }
    }
}

impl ShadowObserver for Fanout {
    fn shadow_dispatched(&self, meta: &ShadowMeta) {
        self.each(|o| o.shadow_dispatched(meta));
    }

    fn comparison(&self, meta: &ShadowMeta, result: &ComparisonResult) {
        self.each(|o| o.comparison(meta, result));
    }

    fn shadow_skipped(&self, meta: &ShadowMeta, reason: SkipReason) {
        self.each(|o| o.shadow_skipped(meta, reason));
    }

    fn shadow_failed(&self, meta: &ShadowMeta, failure: ShadowFailure) {
        self.each(|o| o.shadow_failed(meta, failure));
    }

    fn comparison_skipped(&self, meta: &ShadowMeta, reason: SkipReason) {
        self.each(|o| o.comparison_skipped(meta, reason));
    }
}

/// The production observer: records Prometheus metrics and emits the redacted
/// mismatch log (spec §7.3). The differences it logs are already redacted by the
/// comparison engine.
#[derive(Debug, Default, Clone, Copy)]
pub struct MetricsObserver;

impl MetricsObserver {
    /// Create the production observer.
    pub fn new() -> Self {
        Self
    }
}

impl ShadowObserver for MetricsObserver {
    fn shadow_dispatched(&self, meta: &ShadowMeta) {
        prometheus::shadow_dispatched(&meta.route_id);
        debug!(
            route_id = %meta.route_id,
            request_id = %meta.request_id,
            "limen.shadow_dispatched"
        );
    }

    fn comparison(&self, meta: &ShadowMeta, result: &ComparisonResult) {
        prometheus::comparison(&meta.route_id, result.is_match());
        if result.is_match() {
            debug!(
                route_id = %meta.route_id,
                request_id = %meta.request_id,
                "limen.response_match"
            );
        } else {
            if !result.differences.is_empty() {
                prometheus::diff_sampled(&meta.route_id);
            }
            warn!(
                event = "limen.response_mismatch",
                route_id = %meta.route_id,
                request_id = %meta.request_id,
                method = %meta.method,
                path = %meta.path,
                legacy_status = result.legacy_status,
                new_status = result.new_status,
                status_match = result.status_match,
                body_match = result.body_match,
                diff_truncated = result.diff_truncated,
                differences = result.differences.len(),
                // The differences are pre-redacted by the comparison engine.
                diff = ?result.differences,
                header_mismatches = ?result.header_mismatches,
                // Pre-redacted by the comparison engine: cookie values never
                // appear, only names and attributes.
                cookie_mismatches = ?result.cookie_mismatches,
                location_mismatches = ?result.location_mismatches,
            );
        }
    }

    fn shadow_skipped(&self, meta: &ShadowMeta, reason: SkipReason) {
        prometheus::shadow_skipped(&meta.route_id, reason);
        debug!(
            route_id = %meta.route_id,
            request_id = %meta.request_id,
            reason = reason.as_str(),
            "limen.shadow_skipped"
        );
    }

    fn shadow_failed(&self, meta: &ShadowMeta, failure: ShadowFailure) {
        prometheus::shadow_failed(&meta.route_id, failure);
        debug!(
            route_id = %meta.route_id,
            request_id = %meta.request_id,
            kind = failure.as_str(),
            "limen.shadow_failed"
        );
    }

    fn comparison_skipped(&self, meta: &ShadowMeta, reason: SkipReason) {
        prometheus::comparison_skipped(&meta.route_id, reason);
        debug!(
            route_id = %meta.route_id,
            request_id = %meta.request_id,
            reason = reason.as_str(),
            "limen.comparison_skipped"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics_exporter_prometheus::PrometheusBuilder;

    fn result(body_match: bool) -> ComparisonResult {
        ComparisonResult {
            status_match: true,
            legacy_status: 200,
            new_status: 200,
            body_match,
            diff_kind: None,
            differences: vec![],
            diff_truncated: false,
            header_mismatches: vec![],
            cookie_mismatches: vec![],
            location_mismatches: vec![],
        }
    }

    fn meta() -> ShadowMeta {
        ShadowMeta {
            route_id: "r".to_string(),
            request_id: "req-1".to_string(),
            method: Method::GET,
            path: "/devices/1".to_string(),
        }
    }

    #[test]
    fn metrics_observer_records_match_and_mismatch() {
        // A local recorder captures only this test's emissions (no global state).
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let observer = MetricsObserver::new();
            observer.comparison(&meta(), &result(true));
            observer.comparison(&meta(), &result(false));
            observer.shadow_failed(&meta(), ShadowFailure::Timeout);
            observer.comparison_skipped(&meta(), SkipReason::ResponseTooLarge);
        });
        let rendered = handle.render();
        assert!(rendered.contains("limen_comparisons_total"));
        assert!(rendered.contains(r#"result="match""#));
        assert!(rendered.contains(r#"result="mismatch""#));
        assert!(rendered.contains("limen_shadow_failed_total"));
        assert!(rendered.contains(r#"reason="timeout""#));
        assert!(rendered.contains("limen_comparison_skipped_total"));
        assert!(rendered.contains(r#"reason="response_too_large""#));
    }

    #[test]
    fn fanout_delivers_every_callback_to_every_observer() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct Counting(AtomicUsize);
        impl ShadowObserver for Counting {
            fn shadow_dispatched(&self, _: &ShadowMeta) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            fn comparison(&self, _: &ShadowMeta, _: &ComparisonResult) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            fn shadow_skipped(&self, _: &ShadowMeta, _: SkipReason) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            fn shadow_failed(&self, _: &ShadowMeta, _: ShadowFailure) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            fn comparison_skipped(&self, _: &ShadowMeta, _: SkipReason) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let a = Arc::new(Counting::default());
        let b = Arc::new(Counting::default());
        let fanout = Fanout::new(vec![a.clone(), b.clone()]);
        fanout.shadow_dispatched(&meta());
        fanout.comparison(&meta(), &result(false));
        fanout.shadow_skipped(&meta(), SkipReason::ConcurrencyLimit);
        fanout.shadow_failed(&meta(), ShadowFailure::Error);
        fanout.comparison_skipped(&meta(), SkipReason::ResponseTooLarge);

        assert_eq!(a.0.load(Ordering::Relaxed), 5);
        assert_eq!(b.0.load(Ordering::Relaxed), 5);
    }
}
