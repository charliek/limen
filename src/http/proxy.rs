//! The streaming proxy core: match a route, choose the primary upstream, relay
//! the request, and stream the response back.
//!
//! The default path is zero-copy (spec §3.3): neither the request nor the
//! response body is buffered. When a `shadow_legacy_primary` route samples a
//! request for comparison, the primary (legacy) response is buffered (bounded)
//! so it can be both served to the client *and* compared against a fire-and-
//! forget shadow to the new upstream — the shadow and comparison never delay or
//! affect the client response. On a route that opted a write method into
//! shadowing (`comparison.shadow_methods`), the *request* body is likewise
//! buffered (bounded) so the identical bytes reach both upstreams; that
//! bounded buffering is the only shadow-related work on the client path.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use tracing::{info, info_span, warn, Instrument};
use url::Url;

use super::server::AppState;
use crate::compare::Captured;
use crate::config::model::RouteMode;
use crate::http::body::{self, Buffered};
use crate::http::client::UpstreamClient;
use crate::http::forwarded;
use crate::http::shadow::{self, ShadowRequest};
use crate::observability::request_id::{resolve as resolve_request_id, REQUEST_ID_HEADER};
use crate::observability::{prometheus, SkipReason};
use crate::resilience::BreakerReservation;
use crate::routing::decision::PrimaryDecision;
use crate::routing::{decision, CompiledRoute, Upstream};

/// Hop-by-hop headers (RFC 7230 §6.1) that must not be forwarded across a proxy.
/// Compared lowercased, which is how `HeaderName::as_str` renders them.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The data-plane fallback handler: every client request flows through here.
///
/// This thin wrapper owns the cross-cutting concerns — the in-flight gauge, the
/// request/trace id, the per-request log span, and the request-count/latency
/// metric — and delegates the actual proxying to [`dispatch`], so those are
/// recorded once on every path rather than at each return site.
pub async fn handle(State(state): State<AppState>, req: Request) -> Response {
    let _in_flight = prometheus::in_flight();
    let started = Instant::now();
    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let request_id = resolve_request_id(&parts.headers);

    // Match a route by method + longest path prefix, narrowed by any query
    // conditions the route declares. An unmatched request has no route label, so
    // it is not counted in the per-route request metric.
    let Some(route) =
        state
            .routes()
            .match_route(method.as_str(), parts.uri.path(), parts.uri.query())
    else {
        return finish_response(not_found(), &request_id);
    };
    let route_id = route.id.clone();
    let mode = route.mode;
    let decision = decision::decide_primary(route, &parts.headers, state.flags().as_ref()).await;

    // Inner warnings inherit the request id + route via this span.
    let span = info_span!("request", %request_id, route = %route_id);
    // `dispatch` reports the upstream that actually *served* the client, which
    // differs from the chosen primary when a failover route replays to legacy.
    let (response, served) = dispatch(&state, route, decision, parts, body, &route_id, &request_id)
        .instrument(span)
        .await;

    let status = response.status();
    let latency = started.elapsed();
    prometheus::record_request(
        &route_id,
        method.as_str(),
        served,
        status.as_u16(),
        latency.as_secs_f64(),
    );
    info!(
        %request_id,
        route = %route_id,
        mode = mode.as_str(),
        method = %method,
        upstream = served.as_str(),
        status = status.as_u16(),
        latency_ms = latency.as_millis() as u64,
        "limen.request"
    );
    finish_response(response, &request_id)
}

/// Proxy a matched request to its chosen primary (and, where configured, shadow
/// or fail over). Returns the client response; the caller records metrics/logs.
async fn dispatch(
    state: &AppState,
    route: &CompiledRoute,
    decision: PrimaryDecision,
    parts: Parts,
    body: Body,
    route_id: &str,
    request_id: &str,
) -> (Response, Upstream) {
    let upstream = decision.upstream;
    // `Some` only when a breaker-guarded `new` trial was admitted: we then own
    // the obligation to settle that reserved slot exactly once — `record` on a
    // real attempt below, or `release` if we bail out before reaching new.
    let breaker = decision.breaker;
    let method = parts.method;
    let uri = parts.uri;
    let path = uri.path();

    let base = match upstream {
        Upstream::Legacy => route.legacy_upstream.as_ref(),
        Upstream::New => route.new_upstream.as_ref(),
    };
    let Some(base) = base else {
        release_breaker(&breaker);
        warn!(
            upstream = upstream.as_str(),
            "selected upstream has no configured URL",
        );
        let resp = (
            StatusCode::INTERNAL_SERVER_ERROR,
            "limen: upstream not configured\n",
        )
            .into_response();
        return (resp, upstream);
    };

    // Build the upstream URL, refusing to forward a path we cannot represent
    // byte-for-byte (dot-segments would be silently rewritten — see
    // `build_upstream_url`).
    let Some(url) = build_upstream_url(base, path, uri.query()) else {
        release_breaker(&breaker);
        warn!(
            path,
            "refusing to forward a path that requires normalization"
        );
        let resp = (
            StatusCode::BAD_REQUEST,
            "limen: request path cannot be forwarded unchanged\n",
        )
            .into_response();
        return (resp, upstream);
    };
    let timeout = Duration::from_millis(route.timeouts.primary_ms);
    let mut request_headers = filter_headers(&parts.headers, Direction::Request);
    // Propagate the resolved request id to the upstream (generating one if the
    // client didn't send it); existing trace headers ride along via the copy.
    if let Ok(value) = HeaderValue::from_str(request_id) {
        request_headers.insert(REQUEST_ID_HEADER, value);
    }
    // `X-Forwarded-For`/`X-Forwarded-Proto`, set once here so every upstream
    // request built from `request_headers` below — primary, failover-safe new
    // *and* legacy replay, and (via its header clone) the shadow — carries
    // identical values (spec §6.3, D8). `client_addr` is only populated for
    // real connections (see `forwarded::apply`).
    forwarded::apply(&mut request_headers, client_addr(&parts.extensions));

    // Failover-safe path: a `failover_to_legacy` route sending to new buffers the
    // request body so a new-side failure can be replayed against legacy. Handled
    // before planning a shadow, which `shadow::plan` never produces for this mode
    // anyway — so the shadow setup below is dead work on this path.
    if route.mode == RouteMode::FailoverToLegacy && route.failover_safe && upstream == Upstream::New
    {
        if let Some(legacy_url) = route
            .legacy_upstream
            .as_ref()
            .and_then(|b| build_upstream_url(b, path, uri.query()))
        {
            return failover_dispatch(
                state,
                route_id,
                &breaker,
                method,
                url,
                legacy_url,
                request_headers,
                body,
                timeout,
            )
            .await;
        }
    }

    // Prepare a shadow plan *before* sending the primary, so the method and
    // headers are available to replay. Only `shadow_legacy_primary` + sampled
    // eligible methods, and not while shutting down.
    let planned = if state.is_shutting_down() {
        None
    } else {
        route
            .new_upstream
            .as_ref()
            .and_then(|new_base| build_upstream_url(new_base, path, uri.query()))
            .and_then(|new_url| {
                shadow::plan(route, &method, &request_headers, new_url, &url, request_id)
            })
    };

    // Settle how the body reaches the primary, and whether the shadow survives
    // it (an opted-in write buffers here; see `prepare_request_body`).
    let Some((upstream_body, shadow)) =
        prepare_request_body(state, body, &parts.headers, planned).await
    else {
        release_breaker(&breaker);
        return (unreadable_body(), upstream);
    };

    // The timeout bounds time-to-response (connect + send + first byte), not the
    // body transfer — `send()` resolves once headers arrive, then the body
    // streams without a total deadline, preserving the unbounded streaming path.
    let send = state
        .client()
        .inner()
        .request(method, url)
        .headers(request_headers)
        .body(upstream_body)
        .send();

    match tokio::time::timeout(timeout, send).await {
        Ok(Ok(resp)) => {
            record_breaker(&breaker, !resp.status().is_server_error());
            (primary_succeeded(state, shadow, resp).await, upstream)
        }
        Ok(Err(error)) => {
            // A non-failover-safe failover route returns the new-side failure to
            // the client (the in-flight request is NOT replayed); the breaker
            // still steers *subsequent* requests to legacy.
            record_breaker(&breaker, false);
            prometheus::record_upstream_error(route_id, upstream);
            warn!(upstream = upstream.as_str(), %error, "upstream request failed");
            (bad_gateway(), upstream)
        }
        Err(_elapsed) => {
            record_breaker(&breaker, false);
            prometheus::record_upstream_timeout(route_id, upstream);
            warn!(
                upstream = upstream.as_str(),
                timeout_ms = timeout.as_millis(),
                "upstream did not respond before the primary timeout",
            );
            (gateway_timeout(), upstream)
        }
    }
}

/// Decide how the request body reaches the primary, and finalize the shadow
/// plan around it. Returns `None` only if the client's body errored mid-read,
/// leaving nothing to forward.
///
/// - **No shadow planned** — stream the body straight through (the zero-copy
///   default, spec §3.3).
/// - **Read** — the shadow replays bodyless, so a body-bearing `GET`/`HEAD`
///   simply isn't shadowed; its body could not be reproduced faithfully.
/// - **Opted-in write** (`comparison.shadow_methods`) — buffer the body bounded
///   by `max_body_bytes` and send those same bytes to both upstreams. Only the
///   buffering is on the client path (bounded, as on the failover-safe path);
///   the shadow itself stays fire-and-forget (invariant 2). Over the limit the
///   body is never fully held: it streams to the primary untouched and shadowing
///   is skipped as `request_too_large` (invariant 6). If the shadow limiter is
///   *already* saturated, the buffering is skipped up front rather than paid for
///   a shadow that would be refused anyway.
async fn prepare_request_body(
    state: &AppState,
    body: Body,
    client_headers: &HeaderMap,
    shadow: Option<ShadowRequest>,
) -> Option<(reqwest::Body, Option<ShadowRequest>)> {
    fn streamed(body: Body) -> reqwest::Body {
        reqwest::Body::wrap_stream(body.into_data_stream())
    }

    let Some(mut shadow) = shadow else {
        return Some((streamed(body), None));
    };
    if shadow::method_is_read(&shadow.method) {
        let keep = (!request_has_body(client_headers)).then_some(shadow);
        return Some((streamed(body), keep));
    }

    // Buffering a write's body costs client latency and up to `max_body_bytes`
    // of memory *before* the real permit is taken (which happens only once the
    // primary has responded, in `primary_succeeded`). Paying that for a shadow
    // the limiter would refuse is pure waste under load — exactly when the limit
    // is doing its job — so check for saturation first. The check is
    // best-effort: a slot may free up or fill immediately after, in which case
    // the worst case is either a shadow skipped that could have run, or a body
    // buffered whose shadow is then refused by `try_acquire` — i.e. no worse
    // than the behavior without this gate. `try_acquire` stays authoritative.
    if state.shadow_limiter().is_saturated() {
        state
            .observer()
            .shadow_skipped(&shadow.meta(), SkipReason::ConcurrencyLimit);
        return Some((streamed(body), None));
    }

    match body::buffer_request_or_stream(body, shadow.max_body_bytes).await {
        Buffered::Full(bytes) => {
            shadow.body = Some(bytes.clone());
            Some((reqwest::Body::from(bytes), Some(shadow)))
        }
        Buffered::TooLarge(rest) => {
            // The new upstream is never called (the body could not be buffered
            // for replay), so no comparison is ever attempted — this is a shadow
            // skip, consistent with the concurrency-limit gate above.
            state
                .observer()
                .shadow_skipped(&shadow.meta(), SkipReason::RequestTooLarge);
            Some((streamed(rest), None))
        }
        Buffered::Error => None,
    }
}

/// The client's address, if this request arrived over a real accepted
/// connection. Populated by `Router::into_make_service_with_connect_info`
/// (`src/http/server.rs::serve_with_shutdown`), which inserts a
/// `ConnectInfo<SocketAddr>` extension per connection; integration tests that
/// drive the router via `tower::oneshot` have no such connection and so see
/// `None` here unless a test inserts the extension itself.
///
/// Takes `&Extensions` rather than `&Parts` because `dispatch` partially moves
/// `method`/`uri` out of its `Parts` before this is called — a reference to
/// the whole `Parts` would no longer borrow-check at that point.
fn client_addr(extensions: &axum::http::Extensions) -> Option<std::net::IpAddr> {
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip())
}

/// Echo the resolved request id on the client response so clients and
/// intermediaries can correlate it with Limen's logs.
fn finish_response(mut response: Response, request_id: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    response
}

/// Record an attempt's outcome on the route's breaker reservation, if any.
fn record_breaker(reservation: &Option<BreakerReservation>, success: bool) {
    if let Some(reservation) = reservation {
        reservation.record(success);
    }
}

/// Release a breaker reservation without recording an outcome — used when the
/// request is rejected locally (bad path, missing upstream, un-bufferable body)
/// before any attempt against new is made, so the trial slot is not leaked.
fn release_breaker(reservation: &Option<BreakerReservation>) {
    if let Some(reservation) = reservation {
        reservation.release();
    }
}

/// Failover-safe dispatch: buffer the (bounded) request body, send to new, and —
/// on a new-side failure — replay the same request to legacy. Safe to replay
/// because the route is `failover_safe` (idempotent). Records the new attempt's
/// outcome on the breaker.
///
/// A new-side failure is a 5xx response, a transport error/timeout, **or** a
/// response whose body errors or times out mid-read: because this path buffers
/// the (bounded) new response before committing, such a body failure fails over
/// to legacy rather than streaming a truncated response to the client.
#[allow(clippy::too_many_arguments)]
async fn failover_dispatch(
    state: &AppState,
    route_id: &str,
    breaker: &Option<BreakerReservation>,
    method: Method,
    new_url: Url,
    legacy_url: Url,
    headers: HeaderMap,
    body: Body,
    timeout: Duration,
) -> (Response, Upstream) {
    // Buffer the request body so it can be replayed. failover_safe is opt-in, so
    // an over-limit body that can't be buffered is rejected rather than sent
    // un-replayable.
    let bytes = match axum::body::to_bytes(body, state.request_body_limit()).await {
        Ok(bytes) => bytes,
        Err(_) => {
            // We never reach new on this path, so settle the reserved breaker
            // slot by releasing it (not recording a failure against new).
            release_breaker(breaker);
            let resp = (
                StatusCode::PAYLOAD_TOO_LARGE,
                "limen: request body too large to buffer for failover replay\n",
            )
                .into_response();
            return (resp, Upstream::New);
        }
    };

    let new_result = send_buffered(
        state.client(),
        method.clone(),
        new_url,
        headers.clone(),
        bytes.clone(),
        timeout,
    )
    .await;

    // A transport-level new failure is an upstream error/timeout (a 5xx is a
    // response, counted by the request metric, not an upstream error).
    if let Err(error) = &new_result {
        if error.is_timeout() {
            prometheus::record_upstream_timeout(route_id, Upstream::New);
        } else {
            prometheus::record_upstream_error(route_id, Upstream::New);
        }
    }

    // Classify the new attempt on its *complete* outcome — status and body, not
    // just the status line. On a non-5xx response, buffer the body (bounded) so
    // a 2xx whose body then fails is treated as a new-side failure.
    if let Ok(resp) = new_result {
        if !resp.status().is_server_error() {
            let status = resp.status();
            let resp_headers = filter_headers(resp.headers(), Direction::Response);
            match body::buffer_or_stream(resp, state.request_body_limit()).await {
                Buffered::Full(buffered) => {
                    record_breaker(breaker, true);
                    return (
                        response_from_parts(status, resp_headers, Body::from(buffered)),
                        Upstream::New,
                    );
                }
                Buffered::TooLarge(streamed) => {
                    // Past the buffer bound the body can't be verified, and a
                    // committed stream can't be replayed; relay as-is (the
                    // failover guarantee is header-level for such responses).
                    record_breaker(breaker, true);
                    return (
                        response_from_parts(status, resp_headers, streamed),
                        Upstream::New,
                    );
                }
                // Body errored mid-read (including the total timeout firing):
                // a new-side failure — fall through to the legacy replay.
                Buffered::Error => prometheus::record_upstream_error(route_id, Upstream::New),
            }
        }
    }

    // New failed (5xx, transport error/timeout, or a body that errored
    // mid-read) — replay legacy.
    record_breaker(breaker, false);
    warn!("new upstream failed; failing over to legacy");
    match send_buffered(state.client(), method, legacy_url, headers, bytes, timeout).await {
        Ok(resp) => (relay_response(resp), Upstream::Legacy),
        Err(error) => {
            if error.is_timeout() {
                prometheus::record_upstream_timeout(route_id, Upstream::Legacy);
            } else {
                prometheus::record_upstream_error(route_id, Upstream::Legacy);
            }
            warn!(%error, "legacy failover also failed");
            (bad_gateway(), Upstream::Legacy)
        }
    }
}

/// Send a request with a fully-buffered body, bounding the whole exchange with a
/// total timeout (safe because the body is bounded).
async fn send_buffered(
    client: &UpstreamClient,
    method: Method,
    url: Url,
    headers: HeaderMap,
    body: Bytes,
    timeout: Duration,
) -> Result<reqwest::Response, reqwest::Error> {
    client
        .inner()
        .request(method, url)
        .headers(headers)
        .timeout(timeout)
        .body(reqwest::Body::from(body))
        .send()
        .await
}

/// Handle a successful primary response: stream it directly, or — when the
/// request is shadow-planned — buffer it (bounded) to both serve the client and
/// compare against a fire-and-forget shadow to the new upstream.
async fn primary_succeeded(
    state: &AppState,
    shadow: Option<ShadowRequest>,
    resp: reqwest::Response,
) -> Response {
    // No shadow planned, or shutdown began while the primary was in flight:
    // stream the primary straight through, no buffering, no comparison.
    let Some(shadow_req) = shadow.filter(|_| !state.is_shutting_down()) else {
        return relay_response(resp);
    };
    // Buffering the primary here adds bounded latency on the *sampled* fraction
    // of requests (the documented buffer-for-compare overhead, spec §12) — the
    // shadow dispatch and comparison themselves stay off the client path. A
    // stream-tee that buffers a copy while streaming to the client (zero added
    // latency even when sampled) is a documented future enhancement.
    //
    // Reserve a shadow slot before buffering; if saturated, stream and skip.
    let Some(permit) = state.shadow_limiter().try_acquire() else {
        state
            .observer()
            .shadow_skipped(&shadow_req.meta(), SkipReason::ConcurrencyLimit);
        // The shadow is dead: release the plan — and with it any buffered
        // request body (an opted-in write holds up to `max_body_bytes`) —
        // before the client response, rather than at end of scope.
        drop(shadow_req);
        return relay_response(resp);
    };

    let status = resp.status();
    // Compare against the *unfiltered* upstream headers; filter only for the
    // client-facing response.
    let upstream_headers = resp.headers().clone();
    let client_headers = filter_headers(&upstream_headers, Direction::Response);

    match body::buffer_or_stream(resp, shadow_req.max_body_bytes).await {
        Buffered::Full(bytes) => {
            let legacy = Captured {
                status: status.as_u16(),
                headers: upstream_headers,
                body: bytes.clone(),
                // The legacy request URL, so a relative `Location` resolves
                // against the host that issued it (spec §4.2).
                request_url: Some(shadow_req.legacy_url.clone()),
            };
            shadow::spawn(
                state.client().clone(),
                state.observer(),
                shadow_req,
                legacy,
                permit,
            );
            response_from_parts(status, client_headers, Body::from(bytes))
        }
        Buffered::TooLarge(streamed) => {
            // The primary body is too large to buffer for comparison; serve it
            // (prefix + remaining stream) and skip the comparison.
            state
                .observer()
                .comparison_skipped(&shadow_req.meta(), SkipReason::ResponseTooLarge);
            // Same as above: the shadow will not run, so drop its plan (and any
            // buffered request body) before handing the client its response.
            drop(shadow_req);
            drop(permit);
            response_from_parts(status, client_headers, streamed)
        }
        Buffered::Error => {
            drop(shadow_req);
            drop(permit);
            bad_gateway()
        }
    }
}

/// Build the upstream URL from the upstream origin + the request's path/query.
///
/// The upstream is expected to be an origin (`scheme://host[:port]`). Returns
/// `None` if setting the request path would change it (dot-segment collapse such
/// as `/a/../b`, or a path the URL parser re-encodes) — Limen refuses to forward
/// a rewritten path rather than risk sending the upstream a different resource
/// than the client asked for.
fn build_upstream_url(base: &Url, path: &str, query: Option<&str>) -> Option<Url> {
    let mut url = base.clone();
    url.set_path(path);
    if url.path() != path {
        return None;
    }
    url.set_query(query);
    Some(url)
}

/// Whether headers are being forwarded on the request or response leg.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Request,
    Response,
}

/// Copy headers, dropping:
/// - hop-by-hop headers ([`HOP_BY_HOP`]) and any header named in a `Connection`
///   header's token list (RFC 7230 §6.1);
/// - `transfer-encoding` (a hop-by-hop header) in both directions, since the
///   relay re-frames the body;
/// - on the **request** leg, `host` and `content-length` — the upstream client
///   sets Host and frames the streamed request body itself;
/// - on the **request** leg, a client-supplied `X-Limen-Shadow`
///   unconditionally — Limen is the only party allowed to assert shadow
///   status (`shadow::plan` sets it on the shadow copy only); without this a
///   client could spoof the header on the request it sends and mislead the
///   real upstream into treating primary traffic as a shadow;
/// - on the **response** leg, `X-Forwarded-For`/`X-Forwarded-Proto`/
///   `X-Limen-Shadow` — these are Limen-to-upstream request headers
///   (`http::forwarded`); an upstream that reflects request headers into its
///   response must not leak them onto the client-facing response.
///
/// Response `content-length` is preserved: the body is relayed unchanged, so the
/// length still matches (and `HEAD`/`304` keep their meaningful length).
fn filter_headers(src: &HeaderMap, direction: Direction) -> HeaderMap {
    let connection_named = connection_tokens(src);
    let mut out = HeaderMap::with_capacity(src.len());
    for (name, value) in src {
        let n = name.as_str();
        let drop = HOP_BY_HOP.contains(&n)
            || connection_named.iter().any(|t| t == n)
            || (direction == Direction::Request
                && (n == "host" || n == "content-length" || n == forwarded::X_LIMEN_SHADOW))
            || (direction == Direction::Response
                && (n == forwarded::X_FORWARDED_FOR
                    || n == forwarded::X_FORWARDED_PROTO
                    || n == forwarded::X_LIMEN_SHADOW));
        if drop {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Whether the request carries a body, per its framing headers — a non-zero
/// `content-length` or any `transfer-encoding`. Used to keep a body-bearing
/// GET/HEAD out of shadowing (the shadow replays an empty request).
fn request_has_body(headers: &HeaderMap) -> bool {
    if headers.contains_key("transfer-encoding") {
        return true;
    }
    headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > 0)
}

/// Lowercased header names listed in any `Connection` header's comma-separated
/// token list — these are connection-specific and must not be forwarded.
fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Turn the upstream response into a streamed client response.
fn relay_response(resp: reqwest::Response) -> Response {
    let status = resp.status();
    let headers = filter_headers(resp.headers(), Direction::Response);
    response_from_parts(status, headers, Body::from_stream(resp.bytes_stream()))
}

/// Assemble a client response from a status, headers, and body.
fn response_from_parts(status: StatusCode, headers: HeaderMap, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "limen: no route matched\n").into_response()
}

/// The client's request body errored mid-read while being buffered for replay —
/// there is no complete request to forward, and Limen never invents one.
fn unreadable_body() -> Response {
    (
        StatusCode::BAD_REQUEST,
        "limen: request body could not be read\n",
    )
        .into_response()
}

fn bad_gateway() -> Response {
    (StatusCode::BAD_GATEWAY, "limen: upstream request failed\n").into_response()
}

fn gateway_timeout() -> Response {
    (StatusCode::GATEWAY_TIMEOUT, "limen: upstream timed out\n").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn upstream_url_combines_origin_path_and_query() {
        let base = Url::parse("https://legacy.internal").unwrap();
        let url = build_upstream_url(&base, "/devices/123", Some("verbose=1")).unwrap();
        assert_eq!(
            url.as_str(),
            "https://legacy.internal/devices/123?verbose=1"
        );
    }

    #[test]
    fn upstream_url_without_query() {
        let base = Url::parse("http://localhost:3001").unwrap();
        let url = build_upstream_url(&base, "/health", None).unwrap();
        assert_eq!(url.as_str(), "http://localhost:3001/health");
    }

    #[test]
    fn upstream_url_preserves_percent_encoding() {
        let base = Url::parse("http://h").unwrap();
        let url = build_upstream_url(&base, "/a%20b", Some("q=%2F")).unwrap();
        assert_eq!(url.as_str(), "http://h/a%20b?q=%2F");
    }

    #[test]
    fn upstream_url_refuses_dot_segment_paths() {
        let base = Url::parse("http://h").unwrap();
        // Both literal and percent-encoded dot segments would be rewritten.
        assert!(build_upstream_url(&base, "/a/../admin", None).is_none());
        assert!(build_upstream_url(&base, "/devices/%2e%2e/admin", None).is_none());
    }

    #[test]
    fn filter_drops_hop_by_hop_and_request_only_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("client.example"));
        headers.insert(
            "connection",
            HeaderValue::from_static("keep-alive, x-secret"),
        );
        headers.insert("x-secret", HeaderValue::from_static("leak"));
        headers.insert("content-length", HeaderValue::from_static("5"));
        headers.insert("x-tenant-id", HeaderValue::from_static("t-1"));

        let out = filter_headers(&headers, Direction::Request);
        assert!(out.get("host").is_none());
        assert!(out.get("connection").is_none());
        assert!(out.get("content-length").is_none());
        assert!(out.get("x-secret").is_none()); // named by Connection
        assert_eq!(out.get("x-tenant-id").unwrap(), "t-1");
    }

    #[test]
    fn filter_strips_a_client_forged_shadow_marker_from_the_request_leg() {
        // A client that sends its own `X-Limen-Shadow: 1` must never have it
        // survive onto the primary request Limen builds — only
        // `shadow::plan` may set it, and only on the shadow copy.
        let mut headers = HeaderMap::new();
        headers.insert(forwarded::X_LIMEN_SHADOW, HeaderValue::from_static("1"));
        headers.insert("x-tenant-id", HeaderValue::from_static("t-1"));

        let out = filter_headers(&headers, Direction::Request);
        assert!(out.get(forwarded::X_LIMEN_SHADOW).is_none());
        assert_eq!(out.get("x-tenant-id").unwrap(), "t-1");
    }

    #[test]
    fn filter_strips_forwarded_and_shadow_headers_from_the_response_leg() {
        // An upstream that reflects request headers back must not leak
        // Limen's own to-upstream headers onto the client-facing response.
        let mut headers = HeaderMap::new();
        headers.insert(
            forwarded::X_FORWARDED_FOR,
            HeaderValue::from_static("203.0.113.9"),
        );
        headers.insert(
            forwarded::X_FORWARDED_PROTO,
            HeaderValue::from_static("http"),
        );
        headers.insert(forwarded::X_LIMEN_SHADOW, HeaderValue::from_static("1"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));

        let out = filter_headers(&headers, Direction::Response);
        assert!(out.get(forwarded::X_FORWARDED_FOR).is_none());
        assert!(out.get(forwarded::X_FORWARDED_PROTO).is_none());
        assert!(out.get(forwarded::X_LIMEN_SHADOW).is_none());
        assert_eq!(out.get("content-type").unwrap(), "application/json");
    }

    #[test]
    fn detects_request_body_presence() {
        let mut none = HeaderMap::new();
        assert!(!request_has_body(&none));
        none.insert("content-length", HeaderValue::from_static("0"));
        assert!(!request_has_body(&none));

        let mut with_len = HeaderMap::new();
        with_len.insert("content-length", HeaderValue::from_static("12"));
        assert!(request_has_body(&with_len));

        let mut chunked = HeaderMap::new();
        chunked.insert("transfer-encoding", HeaderValue::from_static("chunked"));
        assert!(request_has_body(&chunked));
    }

    #[test]
    fn filter_preserves_response_content_length_and_host() {
        let mut headers = HeaderMap::new();
        headers.insert("content-length", HeaderValue::from_static("12"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert("transfer-encoding", HeaderValue::from_static("chunked"));
        let out = filter_headers(&headers, Direction::Response);
        assert_eq!(out.get("content-length").unwrap(), "12");
        assert_eq!(out.get("content-type").unwrap(), "application/json");
        assert!(out.get("transfer-encoding").is_none()); // hop-by-hop, re-framed
    }
}
