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
use stridelabs_http::proxy::{
    buffer_or_stream, buffer_or_stream_within, buffer_request_or_stream, build_upstream_url,
    request_has_body, response_from_parts, Buffered, Direction,
};
use tracing::{info, info_span, warn, Instrument};
use url::Url;

use super::server::AppState;
use crate::compare::Captured;
use crate::config::model::RouteMode;
use crate::http::client::UpstreamClient;
use crate::http::forwarded;
use crate::http::shadow::{self, ShadowRequest};
use crate::observability::request_id::{resolve as resolve_request_id, REQUEST_ID_HEADER};
use crate::observability::{prometheus, Observation, ResponseOrigin, SkipReason};
use crate::resilience::{BreakerReservation, ShadowPermit};
use crate::routing::decision::PrimaryDecision;
use crate::routing::{decision, CompiledRoute, Upstream};

/// The debug-gated upstream-attribution response header (`debug.upstream_header`,
/// [`crate::config::model::DebugConfig`]). Only [`handle`] ever sets it — on a
/// relayed response, when the flag is on — but [`filter_headers`] strips any
/// inbound value unconditionally, flag on or off: a client must never make it
/// reach an upstream, and an upstream must never make it reach the client
/// unfiltered (spoof/leak resistance, spec plan 016 W3).
const X_LIMEN_UPSTREAM: &str = "x-limen-upstream";

/// The shortest budget worth replaying legacy in. Below this floor a replay
/// cannot do the job replay exists for: a sub-millisecond attempt is far more
/// likely to record a fresh legacy *timeout* — one that describes the budget
/// that was left, not legacy's actual health — than to complete, so it would
/// misinform the very breaker/steering decisions it is meant to inform. Below
/// the floor the client gets new's own failure instead, exactly as it would
/// with nothing left at all.
const MIN_REPLAY_BUDGET: Duration = Duration::from_millis(10);

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
    // Observe mode's target, captured before `parts` is moved into `dispatch`.
    // Owned copies because the seam is *after* dispatch — the price of profiling
    // the response the client actually received rather than one of the several
    // responses `dispatch` can return without ever reaching its primary arms.
    // Only a profiled deployment pays for the two allocations.
    let observed = state.observe_recorder().map(|_| {
        (
            parts.uri.path().to_string(),
            parts.uri.query().map(str::to_string),
        )
    });
    let decision = decision::decide_primary(
        route,
        &parts.headers,
        state.flags().as_ref(),
        state.fail_safe_mode(),
    )
    .await;

    // Inner warnings inherit the request id + route via this span.
    let span = info_span!("request", %request_id, route = %route_id);
    // `dispatch` reports the upstream that actually *served* the client, which
    // differs from the chosen primary when a failover route replays to legacy.
    let Dispatched {
        mut response,
        served,
        origin,
    } = dispatch(&state, route, decision, parts, body, &route_id, &request_id)
        .instrument(span)
        .await;

    // `debug.upstream_header`: attribute the upstream whose response is being
    // relayed — never the one `dispatch` merely attempted. `filter_headers`
    // has already stripped any inbound `x-limen-upstream` (client-forged on
    // the request leg, upstream-forged on the response leg) by this point, so
    // `insert` here can never collide with a spoofed value.
    if let Some(upstream) = state
        .upstream_header_enabled()
        .then(|| relayed_from(served, origin))
        .flatten()
    {
        response.headers_mut().insert(
            X_LIMEN_UPSTREAM,
            HeaderValue::from_static(upstream.as_str()),
        );
    }

    let status = response.status();
    let latency = started.elapsed();
    prometheus::record_request(
        &route_id,
        method.as_str(),
        served,
        status.as_u16(),
        latency.as_secs_f64(),
    );
    // Observe mode's seam: every response `dispatch` can produce arrives here,
    // so no served response goes unprofiled and none is described by a status
    // other than the one the client saw. Headers are read by reference and the
    // body is never touched, so this cannot delay the client's first byte.
    // `filter_headers` drops hop-by-hop headers only, so `Set-Cookie`,
    // `Location` and `Content-Type` are still here to be counted.
    if let Some((recorder, (path, query))) = state.observe_recorder().zip(observed.as_ref()) {
        recorder.record(
            &route_id,
            Observation::new(
                &method,
                path,
                query.as_deref(),
                status,
                response.headers(),
                origin,
            ),
        );
    }
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

/// One dispatch's outcome: the client response, the upstream that actually
/// served it, and where the response came from.
///
/// `origin` is carried rather than derived because it is the one thing the
/// response cannot tell you — limen's synthesized 502 and an upstream's own 502
/// are identical on the wire. [`handle`]'s observation seam needs the
/// distinction and must not guess it.
struct Dispatched {
    response: Response,
    served: Upstream,
    origin: ResponseOrigin,
}

impl Dispatched {
    /// An upstream answered and limen relayed it.
    fn relayed(response: Response, served: Upstream) -> Self {
        Self {
            response,
            served,
            origin: ResponseOrigin::Upstream,
        }
    }

    /// An upstream was contacted and never answered; the status is limen's own.
    fn silent(response: Response, served: Upstream) -> Self {
        Self {
            response,
            served,
            origin: ResponseOrigin::UpstreamSilent,
        }
    }

    /// limen refused before contacting any upstream.
    fn refused(response: Response, served: Upstream) -> Self {
        Self {
            response,
            served,
            origin: ResponseOrigin::Refused,
        }
    }
}

/// The upstream whose response is being *relayed* to the client, if any —
/// `None` for every limen-synthesized response.
///
/// This is deliberately not `served` alone. `served` names the upstream
/// `dispatch` *attempted* on every path, including the ones where nothing was
/// ever relayed: a transport failure or timeout with no replay
/// (`Dispatched::silent`), a local refusal before any upstream was contacted
/// (`Dispatched::refused`), and a primary success whose buffered body then
/// failed mid-read (`primary_succeeded`'s `Buffered::Error` arm, which still
/// carries the upstream that *answered* in `served` but reports
/// `ResponseOrigin::UpstreamSilent` because the client got limen's own 502).
/// `origin == ResponseOrigin::Upstream` is exactly the fact [`Dispatched`]
/// already tracks for that distinction — including on a failover replay,
/// where `served` is `Upstream::Legacy` because that is whose response the
/// client received, not the `Upstream::New` that was tried first. This
/// function just names the combination for [`handle`], the one caller that
/// turns it into the `x-limen-upstream` header.
fn relayed_from(served: Upstream, origin: ResponseOrigin) -> Option<Upstream> {
    (origin == ResponseOrigin::Upstream).then_some(served)
}

/// Proxy a matched request to its chosen primary (and, where configured, shadow
/// or fail over). Returns the client response; the caller records
/// metrics/logs/observations.
async fn dispatch(
    state: &AppState,
    route: &CompiledRoute,
    decision: PrimaryDecision,
    parts: Parts,
    body: Body,
    route_id: &str,
    request_id: &str,
) -> Dispatched {
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
        return Dispatched::refused(resp, upstream);
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
        return Dispatched::refused(resp, upstream);
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

    // Failover-safe path: a route that has attested idempotence and is sending
    // this request to new buffers the request body so a new-side failure can be
    // replayed against legacy. Both modes that can put new in front of a live
    // legacy qualify — `failover_to_legacy` always, and `percentage_split` on the
    // requests its bucket sent to new. `new_only` is excluded by construction:
    // there is no legacy leg to replay against, so the flag cannot mean anything
    // there. Handled before planning a shadow, which `shadow::plan` produces only
    // for `shadow_legacy_primary` — neither mode here ever shadows, so the shadow
    // setup below is dead work on this path rather than something it skips.
    if route.failover_safe
        && upstream == Upstream::New
        && matches!(
            route.mode,
            RouteMode::FailoverToLegacy | RouteMode::PercentageSplit
        )
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
        return Dispatched::refused(unreadable_body(), upstream);
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

    // One absolute deadline for the whole primary leg, taken immediately before
    // the send. The send below and — on a sampled request — the response
    // buffering that follows it draw down the *same* `primary_ms` budget, so a
    // sampled route's worst-case time to first byte stays ≈ `primary_ms` instead
    // of becoming `primary_ms` plus however long a trickling body cares to take.
    let deadline = tokio::time::Instant::now() + timeout;
    match tokio::time::timeout_at(deadline, send).await {
        Ok(Ok(resp)) => {
            record_breaker(&breaker, !resp.status().is_server_error());
            // `primary_succeeded` reports the origin itself: it can still turn a
            // 2xx upstream response into a synthesized 502 when the buffered
            // body errors mid-read, and that response is not the route's.
            let (response, origin) = primary_succeeded(state, shadow, resp, deadline).await;
            Dispatched {
                response,
                served: upstream,
                origin,
            }
        }
        Ok(Err(error)) => {
            // A non-failover-safe failover route returns the new-side failure to
            // the client (the in-flight request is NOT replayed); the breaker
            // still steers *subsequent* requests to legacy.
            record_breaker(&breaker, false);
            prometheus::record_upstream_error(route_id, upstream);
            warn!(upstream = upstream.as_str(), %error, "upstream request failed");
            Dispatched::silent(bad_gateway(), upstream)
        }
        Err(_elapsed) => {
            record_breaker(&breaker, false);
            prometheus::record_upstream_timeout(route_id, upstream);
            warn!(
                upstream = upstream.as_str(),
                timeout_ms = timeout.as_millis(),
                "upstream did not respond before the primary timeout",
            );
            Dispatched::silent(gateway_timeout(), upstream)
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

    match buffer_request_or_stream(body, shadow.max_body_bytes).await {
        Buffered::Full(bytes) => {
            shadow.body = Some(bytes.clone());
            Some((reqwest::Body::from(bytes), Some(shadow)))
        }
        // `TimedOut` cannot arise on the request leg: no deadline is passed
        // here, deliberately — this buffering is bounded by the client's own
        // upload, and cutting it short would mean sending the primary a body we
        // had already begun to read. Grouped with the over-limit arm because it
        // would mean the same thing: no replayable body, so no shadow.
        Buffered::TooLarge(rest) | Buffered::TimedOut(rest) => {
            // The new upstream is never called (the body could not be buffered
            // for replay), so no comparison is ever attempted — this is a shadow
            // skip, consistent with the concurrency-limit gate above.
            state
                .observer()
                .shadow_skipped(&shadow.meta(), SkipReason::RequestTooLarge);
            Some((streamed(rest), None))
        }
        Buffered::Error => None,
        // `Buffered` is `#[non_exhaustive]`, so a wildcard is mandatory. A
        // variant this build has never heard of is by definition a bounded read
        // that ended some way limen cannot reason about — treated as `Error`,
        // which refuses the request, rather than as a shadow-skip that would
        // serve the client on an outcome nobody checked. Every named arm above
        // is written for a variant the crate can return (`TimedOut` only from
        // the deadline entry point, which this leg does not use); this wildcard
        // alone is unreachable today.
        _ => None,
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
///
/// **Both legs share one `primary_ms` budget.** `timeout` is turned into a
/// single absolute deadline before the new attempt, and the replay gets only
/// what new left of it — the replay is the second leg of *this* client request,
/// not a fresh one, and a client must never wait ~2× the deadline its route
/// declared (`docs/guides/resilience.md`: "one absolute deadline for the whole
/// primary leg"). The practical consequence is that a new attempt that *times
/// out* has spent the budget and is not replayed: the client gets the 504.
/// Failover buys resilience against failures that come back fast — connection
/// refused, connection reset, a prompt 5xx — which is where nearly all of it
/// lives, and it cannot buy it by doubling the latency ceiling.
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
) -> Dispatched {
    // Buffer the request body so it can be replayed. failover_safe is opt-in, so
    // an over-limit body that can't be buffered is rejected rather than sent
    // un-replayable. Neither refusal below reaches new, so both settle the
    // reserved breaker slot by *releasing* it — recording a failure would blame
    // new for a request it never saw, and leaking the slot would wedge a
    // half-open breaker shut.
    //
    // The two refusals are deliberately different statuses, because they are
    // different faults: a body over the limit is the client asking for more
    // than this route buffers (413, and the operator can raise the limit),
    // while a body that errors mid-read is a broken or abandoned upload (400 —
    // the same `unreadable_body` the streaming path returns). Reporting an
    // aborted upload as 413 would send an operator hunting a size limit that
    // was never reached.
    let bytes = match buffer_request_or_stream(body, state.request_body_limit()).await {
        Buffered::Full(bytes) => bytes,
        // `TimedOut` cannot arise — this read passes no deadline — but it would
        // mean what the over-limit arm means: no complete body to replay.
        Buffered::TooLarge(_) | Buffered::TimedOut(_) => {
            release_breaker(breaker);
            let resp = (
                StatusCode::PAYLOAD_TOO_LARGE,
                "limen: request body too large to buffer for failover replay\n",
            )
                .into_response();
            return Dispatched::refused(resp, Upstream::New);
        }
        Buffered::Error => {
            release_breaker(breaker);
            return Dispatched::refused(unreadable_body(), Upstream::New);
        }
        // `Buffered` is `#[non_exhaustive]`; unreachable today. Folded into the
        // unreadable-body refusal rather than the 413, since an unknown outcome
        // is precisely not evidence that a size limit was reached.
        _ => {
            release_breaker(breaker);
            return Dispatched::refused(unreadable_body(), Upstream::New);
        }
    };

    // One absolute budget for the whole exchange, taken after the client's own
    // upload (which is bounded by the client, not by this route) and before the
    // first upstream byte — the same discipline `dispatch` applies on the
    // streaming path.
    let deadline = tokio::time::Instant::now() + timeout;

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
    let new_timed_out = new_result.as_ref().is_err_and(reqwest::Error::is_timeout);
    if new_result.is_err() {
        if new_timed_out {
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
            match buffer_or_stream(resp, state.request_body_limit()).await {
                Buffered::Full(buffered) => {
                    record_breaker(breaker, true);
                    return Dispatched::relayed(
                        response_from_parts(status, resp_headers, Body::from(buffered)),
                        Upstream::New,
                    );
                }
                // `TimedOut` cannot arise here — this leg passes no deadline
                // (`send_buffered` already bounds the whole exchange) — but it
                // means the same thing the over-limit arm does: a body that
                // could not be buffered, so relay it rather than fail over.
                Buffered::TooLarge(streamed) | Buffered::TimedOut(streamed) => {
                    // Past the buffer bound the body can't be verified, and a
                    // committed stream can't be replayed; relay as-is (the
                    // failover guarantee is header-level for such responses).
                    record_breaker(breaker, true);
                    return Dispatched::relayed(
                        response_from_parts(status, resp_headers, streamed),
                        Upstream::New,
                    );
                }
                // Body errored mid-read (including the total timeout firing):
                // a new-side failure — fall through to the legacy replay.
                Buffered::Error => prometheus::record_upstream_error(route_id, Upstream::New),
                // `Buffered` is `#[non_exhaustive]`; unreachable today. An
                // outcome that cannot be shown to be a complete body is not one
                // to relay to the client, so it takes the failure path — which
                // is the conservative direction here, since the legacy replay
                // still serves the client.
                _ => prometheus::record_upstream_error(route_id, Upstream::New),
            }
        }
    }

    // New failed (5xx, transport error/timeout, or a body that errored
    // mid-read) — replay legacy with whatever is left of the one budget.
    record_breaker(breaker, false);
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining < MIN_REPLAY_BUDGET {
        // New spent the whole budget (or all but a sliver of it) before
        // failing — the usual way to get here is a new attempt that timed
        // out. There is not enough time left to replay in without breaking
        // the route's declared deadline — and a replay too small to complete
        // in would just record a *legacy* timeout that describes the budget
        // left over, not legacy's health — so the client gets new's failure.
        // The breaker has already recorded it, so *subsequent* requests are
        // steered to legacy: the route still converges away from a sick new
        // upstream, it just does not do so by doubling this client's wait.
        warn!(
            timeout_ms = timeout.as_millis(),
            remaining_ms = remaining.as_millis(),
            "new upstream failed with the primary budget spent or too small to replay in; not \
             replaying to legacy"
        );
        let resp = if new_timed_out {
            gateway_timeout()
        } else {
            bad_gateway()
        };
        return Dispatched::silent(resp, Upstream::New);
    }
    warn!(
        remaining_ms = remaining.as_millis(),
        "new upstream failed; failing over to legacy"
    );
    match send_buffered(
        state.client(),
        method,
        legacy_url,
        headers,
        bytes,
        remaining,
    )
    .await
    {
        Ok(resp) => Dispatched::relayed(relay_response(resp), Upstream::Legacy),
        Err(error) => {
            if error.is_timeout() {
                prometheus::record_upstream_timeout(route_id, Upstream::Legacy);
            } else {
                prometheus::record_upstream_error(route_id, Upstream::Legacy);
            }
            warn!(%error, "legacy failover also failed");
            Dispatched::silent(bad_gateway(), Upstream::Legacy)
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
/// request is shadow-planned — buffer it (bounded in both size and time) to
/// both serve the client and compare against a fire-and-forget shadow to the
/// new upstream.
///
/// `deadline` is the tail of the route's `primary_ms` budget left over from the
/// send: buffering is the only shadow-related work on the client's response
/// path, so it is the only place a slow body can hold the client, and it must
/// not outlive the budget the route already declared (invariant 2).
///
/// Returns the response's [`ResponseOrigin`] alongside it, because a primary
/// that answered can still leave the client with a response the upstream never
/// sent: buffering for comparison can fail mid-body, and limen then serves its
/// own 502.
async fn primary_succeeded(
    state: &AppState,
    shadow: Option<ShadowRequest>,
    resp: reqwest::Response,
    deadline: tokio::time::Instant,
) -> (Response, ResponseOrigin) {
    // No shadow planned, or shutdown began while the primary was in flight:
    // stream the primary straight through, no buffering, no comparison.
    let Some(shadow_req) = shadow.filter(|_| !state.is_shutting_down()) else {
        return (relay_response(resp), ResponseOrigin::Upstream);
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
        return (relay_response(resp), ResponseOrigin::Upstream);
    };

    let status = resp.status();
    // Compare against the *unfiltered* upstream headers; filter only for the
    // client-facing response.
    let upstream_headers = resp.headers().clone();
    let client_headers = filter_headers(&upstream_headers, Direction::Response);

    // Two responses are known to be unbufferable before a byte is read.
    let eager_skip = if is_event_stream(&upstream_headers) {
        // An event stream never completes by design, so buffering one can only
        // ever end at the deadline: the client pays a stalled first byte *and*
        // the comparison is skipped anyway. Skipping now costs no coverage — no
        // response that would have completed is lost — and keeps it zero-copy.
        Some(SkipReason::EventStream)
    } else if tokio::time::Instant::now() >= deadline {
        // The send may already have spent the whole budget (headers that
        // arrived on the last millisecond of `primary_ms`). Nothing left means
        // nothing to buffer with: demote rather than read a byte past it.
        Some(SkipReason::ResponseBufferTimeout)
    } else {
        None
    };
    if let Some(reason) = eager_skip {
        return demote(
            state,
            shadow_req,
            permit,
            reason,
            status,
            client_headers,
            Body::from_stream(resp.bytes_stream()),
        );
    }

    match buffer_or_stream_within(resp, shadow_req.max_body_bytes, deadline).await {
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
            (
                response_from_parts(status, client_headers, Body::from(bytes)),
                ResponseOrigin::Upstream,
            )
        }
        // Too large to buffer, or too slow to buffer within what was left of
        // `primary_ms`: either way the client is served the complete body
        // (prefix + remaining stream) and the comparison is skipped.
        Buffered::TooLarge(streamed) => demote(
            state,
            shadow_req,
            permit,
            SkipReason::ResponseTooLarge,
            status,
            client_headers,
            streamed,
        ),
        Buffered::TimedOut(streamed) => demote(
            state,
            shadow_req,
            permit,
            SkipReason::ResponseBufferTimeout,
            status,
            client_headers,
            streamed,
        ),
        Buffered::Error => {
            drop(shadow_req);
            drop(permit);
            // The upstream's 2xx is already recorded on the breaker, but the
            // client gets limen's 502 — so the origin is not `Upstream`, or the
            // profile would record a status the client never saw.
            (bad_gateway(), ResponseOrigin::UpstreamSilent)
        }
        // `Buffered` is `#[non_exhaustive]`; unreachable today. The demotion
        // arms above each need the body the variant carries, which an unknown
        // variant cannot be assumed to have — so this takes the `Error` path,
        // the only one that does not depend on holding a body.
        _ => {
            drop(shadow_req);
            drop(permit);
            (bad_gateway(), ResponseOrigin::UpstreamSilent)
        }
    }
}

/// Abandon the comparison for a sampled response and serve `body` as it is —
/// the one demotion shape behind every reason a sampled response cannot be
/// buffered (too large, out of budget, or an event stream that never ends).
///
/// The shadow plan — and with it any buffered request body an opted-in write is
/// holding — and the concurrency permit are dropped *before* the response is
/// returned rather than at end of scope, so the next request's shadow can have
/// the slot immediately.
fn demote(
    state: &AppState,
    shadow_req: ShadowRequest,
    permit: ShadowPermit,
    reason: SkipReason,
    status: StatusCode,
    client_headers: HeaderMap,
    body: Body,
) -> (Response, ResponseOrigin) {
    state
        .observer()
        .comparison_skipped(&shadow_req.meta(), reason);
    drop(shadow_req);
    drop(permit);
    (
        response_from_parts(status, client_headers, body),
        ResponseOrigin::Upstream,
    )
}

/// Whether a response is a server-sent-events stream, by the *essence* of its
/// `Content-Type` — parameters (`; charset=utf-8`) and case are not part of the
/// media type's identity and must not decide this.
fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("text/event-stream"))
}

/// Copy headers, dropping:
/// - everything [`stridelabs_http::proxy::filter_headers`] drops, which is the
///   generic proxy-hop answer and the whole of what limen used to spell out
///   here: hop-by-hop headers
///   ([`HOP_BY_HOP`](stridelabs_http::proxy::HOP_BY_HOP)) and any header named
///   in a `Connection` header's token list (RFC 7230 §6.1), including
///   `transfer-encoding` in both directions since the relay re-frames the body,
///   plus `host` and `content-length` on the **request** leg — the upstream
///   client sets Host and frames the streamed request body itself;
/// - `x-limen-upstream` ([`X_LIMEN_UPSTREAM`]) in **both** directions,
///   unconditionally, whether `debug.upstream_header` is on or off: a client
///   must never make its own value reach an upstream, and an upstream must
///   never make its own value reach the client — only [`handle`], after this
///   filter has already run, is allowed to set it on the client response;
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
///
/// The generic hop rules are the shared crate's; the `x-limen-*` and
/// `X-Forwarded-*` strips above are limen's product policy and are why this
/// wrapper exists at all rather than the crate function being called directly.
/// They are applied by removing the names *after* the generic copy: `remove`
/// takes every field line under a name, so the result is the same map the
/// hand-written single-pass filter produced.
fn filter_headers(src: &HeaderMap, direction: Direction) -> HeaderMap {
    let mut out = stridelabs_http::proxy::filter_headers(src, direction);
    out.remove(X_LIMEN_UPSTREAM);
    match direction {
        Direction::Request => {
            out.remove(forwarded::X_LIMEN_SHADOW);
        }
        Direction::Response => {
            out.remove(forwarded::X_FORWARDED_FOR);
            out.remove(forwarded::X_FORWARDED_PROTO);
            out.remove(forwarded::X_LIMEN_SHADOW);
        }
    }
    out
}

/// Turn the upstream response into a streamed client response.
///
/// Deliberately **not** [`stridelabs_http::proxy::relay_response`], which is
/// otherwise line-for-line this function: the crate's version calls the crate's
/// own generic filter, which knows nothing about `x-limen-upstream`. Adopting it
/// would relay an upstream-forged `x-limen-upstream` straight to the client
/// whenever `debug.upstream_header` is off — the exact leak
/// [`filter_headers`]'s unconditional strip exists to prevent, and the one
/// `tests/upstream_header.rs` pins. What is shared is the assembly underneath
/// ([`response_from_parts`]); the filter choice stays limen's.
fn relay_response(resp: reqwest::Response) -> Response {
    let status = resp.status();
    let headers = filter_headers(resp.headers(), Direction::Response);
    response_from_parts(status, headers, Body::from_stream(resp.bytes_stream()))
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

/// Two kinds of test live here, and the difference matters when reading them.
///
/// The `filter_*` tests exercise limen's own wrapper — the `x-limen-*` and
/// `X-Forwarded-*` strips are product policy and nothing upstream will ever
/// assert them.
///
/// The `upstream_url_*` and `detects_request_body_presence` tests exercise
/// functions that now live in [`stridelabs_http::proxy`]. They were kept when
/// the implementations left rather than deleted along with them: they are the
/// behavior limen *depends on* — a path that would be rewritten is refused, a
/// zero `content-length` is not a body — and keeping them means a future
/// version bump of the shared crate has to survive limen's own statement of
/// that contract, not only the crate's.
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
