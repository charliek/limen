//! Shadow dispatch and comparison (spec §6.1).
//!
//! In `shadow_legacy_primary` mode the client is served by legacy. For an
//! eligible, sampled request, Limen also replays it to the new upstream
//! **fire-and-forget**, buffers both responses, and compares them — entirely off
//! the client path. The shadow timeout, comparison, or any error here can never
//! delay or fail the client response.
//!
//! Eligibility is reads (`GET`/`HEAD`) plus whatever write methods the route
//! opted into via `comparison.shadow_methods` — never writes by default (safety
//! invariant 3). An opted-in write carries its request body here, already
//! buffered (bounded) by [`crate::http::proxy`], so the shadow replays exactly
//! the bytes the primary received.

use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue, Method};
use bytes::Bytes;
use tracing::{info_span, Instrument};
use url::Url;

use crate::compare::{self, diff::DiffLimits, Captured};
use crate::config::model::RouteMode;
use crate::contract::model::ComparisonRules;
use crate::http::body::{self, Buffered};
use crate::http::client::UpstreamClient;
use crate::http::forwarded::X_LIMEN_SHADOW;
use crate::observability::{prometheus, ShadowFailure, ShadowMeta, ShadowObserver, SkipReason};
use crate::resilience::ShadowPermit;
use crate::routing::CompiledRoute;

/// Whether `method` is a read (`GET`/`HEAD`). Reads are always shadow-eligible
/// and replay bodyless (spec §6.1).
pub fn method_is_read(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD)
}

/// Whether `method` may be shadowed on a route whose opted-in write methods are
/// `shadow_methods` (already uppercased). Reads always qualify; a write only
/// when the route named it — writes are never shadowed *by default* (safety
/// invariant 3, spec §6.1).
pub fn method_is_eligible(method: &Method, shadow_methods: &[String]) -> bool {
    method_is_read(method) || shadow_methods.iter().any(|m| m == method.as_str())
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
    /// The request method (a read, or a write the route opted in).
    pub method: Method,
    /// Forwarding headers (already filtered).
    pub headers: HeaderMap,
    /// The buffered request body to replay, byte-identical to the one sent to
    /// the primary. `None` for a read, which is replayed bodyless exactly as
    /// before — the shadow request then frames itself exactly like today's.
    /// Filled in by [`crate::http::proxy`] once the body has been buffered
    /// within `max_body_bytes`.
    pub body: Option<Bytes>,
    /// Timeout bounding the shadow's time-to-response.
    pub shadow_timeout: Duration,
    /// Cap on the buffered comparison body.
    pub max_body_bytes: usize,
    /// Route id (logs/metrics).
    pub route_id: String,
    /// The originating request's resolved `x-request-id`, carried explicitly so
    /// the observer never has to re-parse it out of `headers` (spec §10.1/§10.2).
    pub request_id: String,
    /// Merged behavioral comparison rules.
    pub rules: ComparisonRules,
    /// Diff output bounds.
    pub diff_limits: DiffLimits,
}

impl ShadowRequest {
    /// Build the [`ShadowMeta`] passed to every observer callback for this
    /// shadowed request — cheap identifiers only, never re-derived by the
    /// observer.
    pub fn meta(&self) -> ShadowMeta {
        ShadowMeta {
            route_id: self.route_id.clone(),
            request_id: self.request_id.clone(),
            method: self.method.clone(),
            path: self.new_url.path().to_string(),
        }
    }
}

/// Decide whether to shadow-compare this request and, if so, prepare it. Returns
/// `None` unless the route is `shadow_legacy_primary`, comparison is enabled, the
/// method is eligible (a read, or a write the route opted in), and the request
/// was sampled. The returned plan carries no body yet: the caller buffers one
/// (bounded) for an opted-in write, and may still drop the plan if it cannot.
pub fn plan(
    route: &CompiledRoute,
    method: &Method,
    request_headers: &HeaderMap,
    new_url: Url,
    legacy_url: &Url,
    request_id: &str,
) -> Option<ShadowRequest> {
    if route.mode != RouteMode::ShadowLegacyPrimary || !route.comparison.enabled {
        return None;
    }
    if !method_is_eligible(method, &route.comparison.shadow_methods)
        || !sampled(route.comparison.sample_rate)
    {
        return None;
    }
    // `request_headers` already carries `X-Forwarded-For`/`X-Forwarded-Proto`
    // (set once in `proxy::dispatch` before this is called, spec §6.3, D8);
    // `X-Limen-Shadow` marks *this* copy as the shadow, never the primary —
    // it is added only to this clone, not back onto `request_headers`.
    let mut headers = request_headers.clone();
    headers.insert(X_LIMEN_SHADOW, HeaderValue::from_static("1"));
    Some(ShadowRequest {
        new_url,
        legacy_url: legacy_url.clone(),
        method: method.clone(),
        headers,
        body: None,
        shadow_timeout: Duration::from_millis(route.timeouts.shadow_ms),
        max_body_bytes: route.comparison.max_body_bytes,
        route_id: route.id.clone(),
        request_id: request_id.to_string(),
        rules: route.comparison.rules.clone(),
        diff_limits: DiffLimits::default(),
    })
}

/// Spawn the shadow dispatch + comparison as a fire-and-forget task. Holds the
/// concurrency `permit` and the in-flight gauge for the shadow's lifetime.
/// Never blocks the caller.
pub fn spawn(
    client: UpstreamClient,
    observer: Arc<dyn ShadowObserver>,
    shadow: ShadowRequest,
    legacy: Captured,
    permit: ShadowPermit,
) {
    // Every log line emitted while this shadow runs (including the mismatch
    // warn! in MetricsObserver) carries these ids, so shadow activity for a
    // given client request is correlatable without re-threading fields through
    // every call (spec §10.1/§10.2, D7).
    let request_id = shadow.request_id.clone();
    let route_id = shadow.route_id.clone();
    let span = info_span!("shadow", %request_id, route = %route_id);
    // Taken here, *before* the spawn, and moved into the task: the gauge is up
    // from the moment the shadow is committed to, and comes down on every exit
    // path including a panic unwind (RAII). A drain check reading this gauge
    // must never see a window where a shadow exists but is not counted.
    let in_flight = prometheus::shadow_in_flight();
    tokio::spawn(
        async move {
            let _permit = permit; // released when the shadow completes
            let _in_flight = in_flight;
            run(&client, observer.as_ref(), shadow, legacy).await;
        }
        .instrument(span),
    );
}

async fn run(
    client: &UpstreamClient,
    observer: &dyn ShadowObserver,
    shadow: ShadowRequest,
    legacy: Captured,
) {
    // Built once, up front, from the already-resolved identifiers — the
    // observer never re-derives them (e.g. re-parsing `x-request-id` out of
    // headers).
    let meta = shadow.meta();
    observer.shadow_dispatched(&meta);

    // A *total* request timeout bounds the entire shadow exchange — send AND
    // body read — so a new upstream that sends headers then stalls can never
    // hold the task (and its concurrency permit) open indefinitely. This is
    // safe here precisely because the shadow body is buffered (bounded), unlike
    // the streaming primary path.
    let new_url = shadow.new_url.clone();
    let mut request = client
        .inner()
        .request(shadow.method, shadow.new_url)
        .headers(shadow.headers)
        .timeout(shadow.shadow_timeout);
    // An opted-in write replays the primary's exact bytes. A sized body makes
    // reqwest frame it with a matching `Content-Length` (the client's own
    // `content-length`/`transfer-encoding` were dropped by `filter_headers`), so
    // both upstreams see identical framing. A read sets no body at all, which is
    // byte-for-byte the request Limen sent before write shadowing existed.
    if let Some(body) = shadow.body {
        request = request.body(reqwest::Body::from(body));
    }
    let resp = match request.send().await {
        Ok(resp) => resp,
        Err(e) if e.is_timeout() => return observer.shadow_failed(&meta, ShadowFailure::Timeout),
        Err(_) => return observer.shadow_failed(&meta, ShadowFailure::Error),
    };

    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = match body::buffer_or_stream(resp, shadow.max_body_bytes).await {
        Buffered::Full(body) => body,
        Buffered::TooLarge(_) => {
            return observer.comparison_skipped(&meta, SkipReason::ResponseTooLarge);
        }
        // Includes the total timeout firing mid-body: the stream errors and the
        // permit is released here.
        Buffered::Error => return observer.shadow_failed(&meta, ShadowFailure::Error),
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
    observer.comparison(&meta, &result);
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
    fn reads_are_eligible_and_writes_are_not_by_default() {
        let none: [String; 0] = [];
        assert!(method_is_eligible(&Method::GET, &none));
        assert!(method_is_eligible(&Method::HEAD, &none));
        assert!(!method_is_eligible(&Method::POST, &none));
        assert!(!method_is_eligible(&Method::DELETE, &none));
    }

    #[test]
    fn an_opted_in_write_is_eligible_and_only_that_one() {
        let opted_in = [String::from("POST")];
        assert!(method_is_eligible(&Method::POST, &opted_in));
        // Opting POST in says nothing about any other write.
        assert!(!method_is_eligible(&Method::DELETE, &opted_in));
        assert!(!method_is_eligible(&Method::PUT, &opted_in));
        // Reads stay eligible regardless.
        assert!(method_is_eligible(&Method::GET, &opted_in));
    }

    #[test]
    fn only_reads_replay_bodyless() {
        assert!(method_is_read(&Method::GET));
        assert!(method_is_read(&Method::HEAD));
        assert!(!method_is_read(&Method::POST));
    }
}
