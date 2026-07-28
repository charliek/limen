//! Shadow dispatch and comparison (spec §6.1).
//!
//! In `shadow_legacy_primary` mode the client is served by legacy. For an
//! eligible, sampled read, Limen also replays the request to the new upstream
//! **fire-and-forget**, buffers both responses, and compares them — entirely off
//! the client path. The shadow timeout, comparison, or any error here can never
//! delay or fail the client response.

use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, Method};
use url::Url;

use crate::compare::{self, diff::DiffLimits, Captured};
use crate::config::model::RouteMode;
use crate::contract::model::ComparisonRules;
use crate::http::body::{self, Buffered};
use crate::http::client::UpstreamClient;
use crate::observability::{ShadowFailure, ShadowObserver, SkipReason};
use crate::resilience::ShadowPermit;
use crate::routing::CompiledRoute;

/// Whether `method` is a shadow-eligible read. Only `GET`/`HEAD` (spec §6.1);
/// writes are never shadowed by default.
pub fn method_is_eligible(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD)
}

/// Per-request sampling decision (spec §3.3). `>= 1.0` always samples, `<= 0.0`
/// never.
pub fn sampled(sample_rate: f64) -> bool {
    if sample_rate <= 0.0 {
        return false;
    }
    if sample_rate >= 1.0 {
        return true;
    }
    rand::random::<f64>() < sample_rate
}

/// A prepared shadow request, captured *before* the primary is sent so the
/// method and headers are available to replay to the new upstream.
pub struct ShadowRequest {
    /// The new upstream URL (origin + the request's path/query).
    pub new_url: Url,
    /// The legacy upstream URL the primary request was sent to. Carried purely
    /// so the comparison can resolve each side's relative `Location` against
    /// its own request URL (spec §4.2).
    pub legacy_url: Url,
    /// The request method (always GET/HEAD).
    pub method: Method,
    /// Forwarding headers (already filtered).
    pub headers: HeaderMap,
    /// Timeout bounding the shadow's time-to-response.
    pub shadow_timeout: Duration,
    /// Cap on the buffered comparison body.
    pub max_body_bytes: usize,
    /// Route id (logs/metrics).
    pub route_id: String,
    /// Merged behavioral comparison rules.
    pub rules: ComparisonRules,
    /// Diff output bounds.
    pub diff_limits: DiffLimits,
}

/// Decide whether to shadow-compare this request and, if so, prepare it. Returns
/// `None` unless the route is `shadow_legacy_primary`, comparison is enabled, the
/// method is an eligible read, and the request was sampled.
pub fn plan(
    route: &CompiledRoute,
    method: &Method,
    request_headers: &HeaderMap,
    new_url: Url,
    legacy_url: &Url,
) -> Option<ShadowRequest> {
    if route.mode != RouteMode::ShadowLegacyPrimary || !route.comparison.enabled {
        return None;
    }
    if !method_is_eligible(method) || !sampled(route.comparison.sample_rate) {
        return None;
    }
    Some(ShadowRequest {
        new_url,
        legacy_url: legacy_url.clone(),
        method: method.clone(),
        headers: request_headers.clone(),
        shadow_timeout: Duration::from_millis(route.timeouts.shadow_ms),
        max_body_bytes: route.comparison.max_body_bytes,
        route_id: route.id.clone(),
        rules: route.comparison.rules.clone(),
        diff_limits: DiffLimits::default(),
    })
}

/// Spawn the shadow dispatch + comparison as a fire-and-forget task. Holds the
/// concurrency `permit` for the shadow's lifetime. Never blocks the caller.
pub fn spawn(
    client: UpstreamClient,
    observer: Arc<dyn ShadowObserver>,
    shadow: ShadowRequest,
    legacy: Captured,
    permit: ShadowPermit,
) {
    tokio::spawn(async move {
        let _permit = permit; // released when the shadow completes
        run(&client, observer.as_ref(), shadow, legacy).await;
    });
}

async fn run(
    client: &UpstreamClient,
    observer: &dyn ShadowObserver,
    shadow: ShadowRequest,
    legacy: Captured,
) {
    observer.shadow_dispatched(&shadow.route_id);

    // A *total* request timeout bounds the entire shadow exchange — send AND
    // body read — so a new upstream that sends headers then stalls can never
    // hold the task (and its concurrency permit) open indefinitely. This is
    // safe here precisely because the shadow body is buffered (bounded), unlike
    // the streaming primary path.
    let new_url = shadow.new_url.clone();
    let result = client
        .inner()
        .request(shadow.method, shadow.new_url)
        .headers(shadow.headers)
        .timeout(shadow.shadow_timeout)
        .send()
        .await;
    let resp = match result {
        Ok(resp) => resp,
        Err(e) if e.is_timeout() => {
            return observer.shadow_failed(&shadow.route_id, ShadowFailure::Timeout)
        }
        Err(_) => return observer.shadow_failed(&shadow.route_id, ShadowFailure::Error),
    };

    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = match body::buffer_or_stream(resp, shadow.max_body_bytes).await {
        Buffered::Full(body) => body,
        Buffered::TooLarge(_) => {
            return observer.comparison_skipped(&shadow.route_id, SkipReason::ResponseTooLarge);
        }
        // Includes the total timeout firing mid-body: the stream errors and the
        // permit is released here.
        Buffered::Error => return observer.shadow_failed(&shadow.route_id, ShadowFailure::Error),
    };

    let new = Captured {
        status,
        headers,
        body,
        // Each side resolves its own relative `Location` against its own
        // request URL, so a legacy `/next` and a new `https://new/next` are
        // recognized as the same target (spec §4.2).
        request_url: Some(new_url),
    };
    let result = compare::compare(&shadow.rules, &shadow.diff_limits, &legacy, &new);
    observer.comparison(&shadow.route_id, &result);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_bounds() {
        assert!(!sampled(0.0));
        assert!(sampled(1.0));
        assert!(sampled(2.0));
        assert!(!sampled(-1.0));
    }

    #[test]
    fn eligible_methods() {
        assert!(method_is_eligible(&Method::GET));
        assert!(method_is_eligible(&Method::HEAD));
        assert!(!method_is_eligible(&Method::POST));
        assert!(!method_is_eligible(&Method::DELETE));
    }
}
