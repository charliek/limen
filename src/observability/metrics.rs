//! Comparison and shadow statistics + the observer that records them.
//!
//! Phase 4 records outcomes into in-memory atomic counters via a
//! [`ShadowObserver`]; the production [`MetricsObserver`] also emits the
//! redacted mismatch log (spec §7.3). Phase 7 renders these counters on the
//! control-plane `/metrics` endpoint and adds the request-level metric set.
//!
//! The observer indirection keeps the shadow path testable: a test can supply a
//! capturing observer to assert on comparison outcomes without scraping logs.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::Serialize;
use tracing::{debug, warn};

use crate::compare::result::ComparisonResult;

/// Why a shadow or comparison was skipped (low-cardinality metric labels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The shadow concurrency limit was saturated.
    ConcurrencyLimit,
    /// A response body exceeded `max_body_bytes`.
    ResponseTooLarge,
}

impl SkipReason {
    /// A stable, lowercase label.
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::ConcurrencyLimit => "concurrency_limit",
            SkipReason::ResponseTooLarge => "response_too_large",
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
    /// A stable, lowercase label.
    pub fn as_str(self) -> &'static str {
        match self {
            ShadowFailure::Timeout => "timeout",
            ShadowFailure::Error => "error",
        }
    }
}

/// Receives shadow and comparison outcomes off the client path.
pub trait ShadowObserver: Send + Sync {
    /// A point-in-time snapshot of the recorded counters (the control-plane
    /// `/metrics` endpoint reads this in Phase 7). Observers that don't track
    /// counters return [`StatsSnapshot::default`].
    fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot::default()
    }
    /// A shadow request was dispatched to the new upstream.
    fn shadow_dispatched(&self, route_id: &str);
    /// A comparison completed (match or mismatch).
    fn comparison(&self, route_id: &str, result: &ComparisonResult);
    /// A shadow request was not dispatched.
    fn shadow_skipped(&self, route_id: &str, reason: SkipReason);
    /// A shadow request was dispatched but failed.
    fn shadow_failed(&self, route_id: &str, failure: ShadowFailure);
    /// A comparison was not performed (e.g. a response was too large to buffer).
    fn comparison_skipped(&self, route_id: &str, reason: SkipReason);
}

/// In-memory counters for shadow and comparison activity.
#[derive(Debug, Default)]
pub struct Stats {
    /// Shadow requests dispatched to the new upstream.
    pub shadow_dispatched: AtomicU64,
    /// Shadows skipped because the concurrency limit was saturated.
    pub shadow_skipped_concurrency: AtomicU64,
    /// Shadow requests that timed out.
    pub shadow_timeouts: AtomicU64,
    /// Shadow requests that errored (non-timeout).
    pub shadow_errors: AtomicU64,
    /// Comparisons whose responses matched.
    pub comparison_match: AtomicU64,
    /// Comparisons whose responses differed.
    pub comparison_mismatch: AtomicU64,
    /// Comparisons skipped because a body exceeded `max_body_bytes`.
    pub comparison_skipped_too_large: AtomicU64,
}

impl Stats {
    /// A point-in-time copy of the counters.
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            shadow_dispatched: self.shadow_dispatched.load(Ordering::Relaxed),
            shadow_skipped_concurrency: self.shadow_skipped_concurrency.load(Ordering::Relaxed),
            shadow_timeouts: self.shadow_timeouts.load(Ordering::Relaxed),
            shadow_errors: self.shadow_errors.load(Ordering::Relaxed),
            comparison_match: self.comparison_match.load(Ordering::Relaxed),
            comparison_mismatch: self.comparison_mismatch.load(Ordering::Relaxed),
            comparison_skipped_too_large: self.comparison_skipped_too_large.load(Ordering::Relaxed),
        }
    }
}

/// A plain, serializable snapshot of [`Stats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct StatsSnapshot {
    /// Shadow requests dispatched.
    pub shadow_dispatched: u64,
    /// Shadows skipped due to the concurrency limit.
    pub shadow_skipped_concurrency: u64,
    /// Shadow timeouts.
    pub shadow_timeouts: u64,
    /// Shadow errors.
    pub shadow_errors: u64,
    /// Matching comparisons.
    pub comparison_match: u64,
    /// Mismatching comparisons.
    pub comparison_mismatch: u64,
    /// Comparisons skipped (too large).
    pub comparison_skipped_too_large: u64,
}

/// The production observer: increments [`Stats`] and emits the redacted
/// mismatch log (spec §7.3). The differences it logs are already redacted by the
/// comparison engine.
pub struct MetricsObserver {
    stats: Arc<Stats>,
}

impl MetricsObserver {
    /// Create an observer recording into `stats`.
    pub fn new(stats: Arc<Stats>) -> Self {
        Self { stats }
    }
}

impl ShadowObserver for MetricsObserver {
    fn snapshot(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }

    fn shadow_dispatched(&self, route_id: &str) {
        self.stats.shadow_dispatched.fetch_add(1, Ordering::Relaxed);
        debug!(route_id, "limen.shadow_dispatched");
    }

    fn comparison(&self, route_id: &str, result: &ComparisonResult) {
        if result.is_match() {
            self.stats.comparison_match.fetch_add(1, Ordering::Relaxed);
            debug!(route_id, "limen.response_match");
        } else {
            self.stats
                .comparison_mismatch
                .fetch_add(1, Ordering::Relaxed);
            warn!(
                event = "limen.response_mismatch",
                route_id,
                legacy_status = result.legacy_status,
                new_status = result.new_status,
                status_match = result.status_match,
                body_match = result.body_match,
                diff_truncated = result.diff_truncated,
                differences = result.differences.len(),
                // The differences are pre-redacted by the comparison engine.
                diff = ?result.differences,
                header_mismatches = ?result.header_mismatches,
            );
        }
    }

    fn shadow_skipped(&self, route_id: &str, reason: SkipReason) {
        if reason == SkipReason::ConcurrencyLimit {
            self.stats
                .shadow_skipped_concurrency
                .fetch_add(1, Ordering::Relaxed);
        }
        debug!(route_id, reason = reason.as_str(), "limen.shadow_skipped");
    }

    fn shadow_failed(&self, route_id: &str, failure: ShadowFailure) {
        match failure {
            ShadowFailure::Timeout => self.stats.shadow_timeouts.fetch_add(1, Ordering::Relaxed),
            ShadowFailure::Error => self.stats.shadow_errors.fetch_add(1, Ordering::Relaxed),
        };
        debug!(route_id, kind = failure.as_str(), "limen.shadow_failed");
    }

    fn comparison_skipped(&self, route_id: &str, reason: SkipReason) {
        if reason == SkipReason::ResponseTooLarge {
            self.stats
                .comparison_skipped_too_large
                .fetch_add(1, Ordering::Relaxed);
        }
        debug!(
            route_id,
            reason = reason.as_str(),
            "limen.comparison_skipped"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_observer_counts_match_and_mismatch() {
        let stats = Arc::new(Stats::default());
        let observer = MetricsObserver::new(stats.clone());

        let matched = ComparisonResult {
            status_match: true,
            legacy_status: 200,
            new_status: 200,
            body_match: true,
            diff_kind: None,
            differences: vec![],
            diff_truncated: false,
            header_mismatches: vec![],
        };
        let mismatched = ComparisonResult {
            body_match: false,
            ..matched.clone()
        };

        observer.comparison("r", &matched);
        observer.comparison("r", &mismatched);
        observer.shadow_failed("r", ShadowFailure::Timeout);
        observer.comparison_skipped("r", SkipReason::ResponseTooLarge);

        let snap = stats.snapshot();
        assert_eq!(snap.comparison_match, 1);
        assert_eq!(snap.comparison_mismatch, 1);
        assert_eq!(snap.shadow_timeouts, 1);
        assert_eq!(snap.comparison_skipped_too_large, 1);
    }
}
