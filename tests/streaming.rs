//! Streaming safety on the *sampled* comparison path.
//!
//! The zero-copy relay has never had a problem with a body that takes its time:
//! it is forwarded chunk by chunk and the client sees the first byte the moment
//! the upstream sends it. Buffering for comparison is different — it holds the
//! client's response until the body *completes*, and a chunked or event-stream
//! response below `max_body_bytes` may never complete at all. These tests pin
//! both halves of the fix: an event stream is skipped before a byte is
//! buffered, and every other sampled body is buffered only within what is left
//! of the route's `primary_ms` budget.
//!
//! Every upstream here is a `common::raw_upstream` rather than a `wiremock`
//! mock: proving these properties needs response shapes no mock template can
//! produce — a body held half-written, a stream that never ends, a socket
//! closed mid-body. Every test is self-bounding, so a regression fails it
//! rather than hanging it.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use axum::Router;
use bytes::Bytes;
use common::{config_from_yaml, metric_value, raw_upstream, send, write, Gate};
use futures::{Stream, StreamExt};
use limen::compare::result::ComparisonResult;
use limen::config::model::Config;
use limen::health::endpoints::ControlState;
use limen::http::server::{build_state_with_observer, control_plane_router, data_plane_router};
use limen::observability::{
    Fanout, MetricsObserver, ShadowFailure, ShadowMeta, ShadowObserver, SkipReason,
};

/// The outer bound on anything that should already have happened. Generous on
/// purpose: it exists so a regression *fails* instead of hanging, not to assert
/// anything about latency.
const BOUND: Duration = Duration::from_secs(10);

/// A short primary budget for the tests that want the buffering deadline to
/// fire while the upstream is still mid-body.
const SHORT_PRIMARY_MS: u64 = 300;

/// The budget for everything that must *not* demote. Seconds rather than
/// [`SHORT_PRIMARY_MS`] on purpose: a control asserting "this still compared"
/// would otherwise be one loaded CI runner away from a legitimate demotion, and
/// would fail for a reason that has nothing to do with what it is pinning.
const GENEROUS_PRIMARY_MS: u64 = 5_000;

// --- observer ------------------------------------------------------------

/// Records comparison outcomes so a test can assert on them without racing the
/// Prometheus exposition. Skips are tagged with the callback that produced them
/// (`shadow_skipped` and `comparison_skipped` share the `SkipReason`
/// vocabulary but are different counters).
#[derive(Clone, Default)]
struct Capture {
    skips: Arc<Mutex<Vec<String>>>,
    comparisons: Arc<Mutex<usize>>,
}

impl ShadowObserver for Capture {
    fn shadow_dispatched(&self, _meta: &ShadowMeta) {}
    fn comparison(&self, _meta: &ShadowMeta, _result: &ComparisonResult) {
        *self.comparisons.lock().unwrap() += 1;
    }
    fn shadow_skipped(&self, _meta: &ShadowMeta, reason: SkipReason) {
        self.push(format!("shadow_skipped:{}", reason.as_str()));
    }
    fn shadow_failed(&self, _meta: &ShadowMeta, _failure: ShadowFailure) {}
    fn comparison_skipped(&self, _meta: &ShadowMeta, reason: SkipReason) {
        self.push(format!("comparison_skipped:{}", reason.as_str()));
    }
}

impl Capture {
    fn push(&self, entry: String) {
        self.skips.lock().unwrap().push(entry);
    }
    fn skips(&self) -> Vec<String> {
        self.skips.lock().unwrap().clone()
    }
    fn comparisons(&self) -> usize {
        *self.comparisons.lock().unwrap()
    }
    /// Wait (bounded) for the observer to record `entry`, then return.
    async fn expect(&self, entry: &str) {
        common::wait_until(entry, || self.skips().iter().any(|s| s == entry)).await;
    }
}

// --- wiring --------------------------------------------------------------

/// Both planes over one state, with the production metrics observer *and* the
/// test capture — the metric labels are part of the contract these tests pin,
/// so neither observer may replace the other.
///
/// `prometheus::install()` hands back one process-wide recorder shared by every
/// test in this binary, so each test below uses a route id no other test uses.
fn planes(cfg: &Config, capture: &Capture) -> (Router, Router) {
    let handle = limen::observability::prometheus::install();
    let observer: Arc<dyn ShadowObserver> = Arc::new(Fanout::new(vec![
        Arc::new(MetricsObserver::new()),
        Arc::new(capture.clone()),
    ]));
    let state = build_state_with_observer(cfg, Path::new("."), observer).expect("build state");
    let data = data_plane_router(state.clone());
    let control = ControlState::new(state.flags().clone(), state.routes_arc(), handle);
    (data, control_plane_router(control, "/metrics"))
}

/// A route that only relays: no comparison, so no buffering ever.
fn relay_config(id: &str, legacy: &str) -> Config {
    config_from_yaml(&format!(
        r#"
routes:
  - id: {id}
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    mode: legacy_only
"#
    ))
}

/// A route that samples every eligible request for comparison, with `primary_ms`
/// as the whole budget for send *and* response buffering.
fn sampled_config(id: &str, legacy: &str, new: &str, primary_ms: u64) -> Config {
    config_from_yaml(&format!(
        r#"
routes:
  - id: {id}
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{new}"
    mode: shadow_legacy_primary
    timeouts: {{ primary_ms: {primary_ms}, shadow_ms: 2000 }}
    comparison: {{ enabled: true, sample_rate: 1.0, max_body_bytes: 262144 }}
"#
    ))
}

async fn get(app: &Router, uri: &str) -> axum::http::Response<Body> {
    tokio::time::timeout(
        BOUND,
        send(
            app,
            Request::builder().uri(uri).body(Body::empty()).unwrap(),
        ),
    )
    .await
    .expect("the response head must not wait on the whole body")
}

/// The exposition text, for the label assertions.
async fn metrics(control: &Router) -> String {
    let resp = send(
        control,
        Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

// --- raw upstreams --------------------------------------------------------

/// One `Transfer-Encoding: chunked` frame.
fn chunk(data: &str) -> String {
    format!("{:x}\r\n{data}\r\n", data.len())
}

/// A chunked response head with no `Content-Length`. `Connection: close` keeps
/// the client from pooling the socket, so each request gets a fresh handler
/// (limen strips the header on the response leg, so the test client never sees
/// it).
fn chunked_head(content_type: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    )
}

/// A chunked upstream that sends `AAAAAAAAAA`, then waits on `gate` before
/// sending `BBBBBBBBBB` and ending the stream. Never sends a `Content-Length`,
/// so nothing about the response reveals how much is still to come.
async fn chunked_upstream(gate: Gate) -> String {
    raw_upstream(move |mut sock, _head| {
        let gate = gate.clone();
        async move {
            write(&mut sock, &chunked_head("text/plain")).await;
            write(&mut sock, &chunk("AAAAAAAAAA")).await;
            gate.wait().await;
            write(&mut sock, &chunk("BBBBBBBBBB")).await;
            write(&mut sock, "0\r\n\r\n").await;
        }
    })
    .await
}

/// An SSE upstream: drips `data:` frames and never completes until `gate`
/// opens — the shape of every real event stream, which is exactly why
/// buffering one for comparison can only ever end at the deadline.
async fn event_stream_upstream(gate: Gate) -> String {
    raw_upstream(move |mut sock, _head| {
        let gate = gate.clone();
        async move {
            // A charset parameter, so the skip has to match on the media type's
            // essence rather than the raw header value.
            write(&mut sock, &chunked_head("text/event-stream; charset=utf-8")).await;
            write(&mut sock, &chunk("data: one\n\n")).await;
            gate.wait().await;
            write(&mut sock, &chunk("data: two\n\n")).await;
            write(&mut sock, "0\r\n\r\n").await;
        }
    })
    .await
}

/// A chunked upstream that completes immediately — the control for every
/// demotion test here.
async fn complete_upstream(body: &'static str) -> String {
    raw_upstream(move |mut sock, _head| async move {
        write(&mut sock, &chunked_head("text/plain")).await;
        write(&mut sock, &chunk(body)).await;
        write(&mut sock, "0\r\n\r\n").await;
    })
    .await
}

// --- helpers on the client side ------------------------------------------

/// Read from the client's body stream until at least `n` bytes have arrived,
/// bounded so a proxy that buffered the whole response fails the test rather
/// than hanging it. Loops because chunk boundaries on the wire are not the
/// proxy's to preserve.
async fn read_at_least<S>(chunks: &mut S, n: usize, what: &str) -> Vec<u8>
where
    S: Stream<Item = Result<Bytes, axum::Error>> + Unpin,
{
    let mut out = Vec::new();
    while out.len() < n {
        let chunk = tokio::time::timeout(BOUND, chunks.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .unwrap_or_else(|| panic!("the body ended before {what}"))
            .expect("chunk read");
        out.extend_from_slice(&chunk);
    }
    out
}

/// Drain the rest of a client body stream (bounded).
async fn drain<S>(mut chunks: S) -> Vec<u8>
where
    S: Stream<Item = Result<Bytes, axum::Error>> + Unpin,
{
    let rest = async {
        let mut out = Vec::new();
        while let Some(chunk) = chunks.next().await {
            out.extend_from_slice(&chunk.expect("chunk read"));
        }
        out
    };
    tokio::time::timeout(BOUND, rest)
        .await
        .expect("the body must finish once the upstream is released")
}

// --- the relay baseline ---------------------------------------------------

#[tokio::test]
async fn an_unsampled_chunked_response_reaches_the_client_before_the_upstream_finishes_it() {
    let gate = Gate::new();
    let upstream = chunked_upstream(gate.clone()).await;
    let (data, _control) = planes(
        &relay_config("relay-chunked", &upstream),
        &Capture::default(),
    );

    let resp = get(&data, "/things/1").await;
    assert_eq!(resp.status(), 200);

    // The load-bearing assertion: the first chunk is in the client's hands
    // while the upstream still owes the last one. `gate` only opens after.
    let mut chunks = resp.into_body().into_data_stream();
    let first = read_at_least(&mut chunks, 10, "the first chunk").await;
    assert_eq!(first, b"AAAAAAAAAA");

    gate.open();
    assert_eq!(drain(chunks).await, b"BBBBBBBBBB");
}

#[tokio::test]
async fn an_unsampled_event_stream_reaches_the_client_before_the_upstream_finishes_it() {
    let gate = Gate::new();
    let upstream = event_stream_upstream(gate.clone()).await;
    let (data, _control) = planes(&relay_config("relay-sse", &upstream), &Capture::default());

    let resp = get(&data, "/events").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/event-stream; charset=utf-8"
    );

    let mut chunks = resp.into_body().into_data_stream();
    let first = read_at_least(&mut chunks, "data: one\n\n".len(), "the first event").await;
    assert_eq!(first, b"data: one\n\n");

    gate.open();
    assert_eq!(drain(chunks).await, b"data: two\n\n");
}

// --- the sampled path -----------------------------------------------------

#[tokio::test]
async fn a_sampled_event_stream_is_skipped_before_a_byte_is_buffered() {
    let gate = Gate::new();
    let legacy = event_stream_upstream(gate.clone()).await;
    // A raw upstream that records every connection, so "the shadow never ran"
    // is asserted against the new upstream itself rather than inferred.
    let contacted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = contacted.clone();
    let new = raw_upstream(move |mut sock, _head| {
        let seen = seen.clone();
        async move {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            write(&mut sock, &chunked_head("text/plain")).await;
            write(&mut sock, "0\r\n\r\n").await;
        }
    })
    .await;

    let capture = Capture::default();
    // A budget long enough that a *buffering* proxy would visibly park: the
    // bounded reads below are shorter than it, so the eager skip is what makes
    // this test pass rather than the deadline rescuing it.
    let cfg = sampled_config("sampled-sse", &legacy, &new, GENEROUS_PRIMARY_MS);
    let (data, control) = planes(&cfg, &capture);

    let resp = tokio::time::timeout(Duration::from_secs(2), get(&data, "/events"))
        .await
        .expect("an event stream must not be buffered");
    assert_eq!(resp.status(), 200);

    let mut chunks = resp.into_body().into_data_stream();
    let first = tokio::time::timeout(
        Duration::from_secs(2),
        read_at_least(&mut chunks, "data: one\n\n".len(), "the first event"),
    )
    .await
    .expect("the first event must arrive without waiting on the stream to end");
    assert_eq!(first, b"data: one\n\n");

    capture.expect("comparison_skipped:event_stream").await;
    assert_eq!(
        metric_value(
            &metrics(&control).await,
            r#"limen_comparison_skipped_total{route="sampled-sse",reason="event_stream"}"#
        ),
        Some(1.0)
    );
    // The comparison was abandoned before the shadow was ever dispatched: an
    // event stream can never be compared, so replaying it to new would be pure
    // load on the upstream limen is trying to protect.
    assert_eq!(contacted.load(std::sync::atomic::Ordering::SeqCst), 0);

    gate.open();
    assert_eq!(drain(chunks).await, b"data: two\n\n");
}

#[tokio::test]
async fn a_sampled_trickling_body_demotes_at_the_deadline_and_still_delivers_every_byte() {
    let gate = Gate::new();
    let legacy = chunked_upstream(gate.clone()).await;
    let new = complete_upstream("AAAAAAAAAABBBBBBBBBB").await;

    let capture = Capture::default();
    let cfg = sampled_config("sampled-trickle", &legacy, &new, SHORT_PRIMARY_MS);
    let (data, control) = planes(&cfg, &capture);

    // A length-less body under `max_body_bytes` that never completes: without a
    // deadline inside the buffering, this call never returns.
    let resp = get(&data, "/trickle").await;
    assert_eq!(resp.status(), 200);
    capture
        .expect("comparison_skipped:response_buffer_timeout")
        .await;
    assert_eq!(
        metric_value(
            &metrics(&control).await,
            r#"limen_comparison_skipped_total{route="sampled-trickle",reason="response_buffer_timeout"}"#
        ),
        Some(1.0)
    );

    // The prefix already read into the buffer is handed over exactly once, and
    // the tail still follows it — a demotion must cost the comparison, never a
    // byte of the client's body.
    let mut chunks = resp.into_body().into_data_stream();
    let first = read_at_least(&mut chunks, 10, "the buffered prefix").await;
    gate.open();
    let mut whole = first;
    whole.extend_from_slice(&drain(chunks).await);
    assert_eq!(whole, b"AAAAAAAAAABBBBBBBBBB");
}

#[tokio::test]
async fn a_sampled_body_that_completes_within_the_budget_is_still_compared() {
    // The false-demotion control: the deadline must not cost a comparison that
    // the buffering could have finished. Deliberately on the generous budget —
    // what is being pinned is that *having* a deadline does not demote a body
    // that arrives, not how tight the budget can be cut.
    let legacy = complete_upstream("hello").await;
    let new = complete_upstream("hello").await;

    let capture = Capture::default();
    let cfg = sampled_config("sampled-fast", &legacy, &new, GENEROUS_PRIMARY_MS);
    let (data, control) = planes(&cfg, &capture);

    let resp = get(&data, "/fast").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(drain(resp.into_body().into_data_stream()).await, b"hello");

    common::wait_until("the comparison", || capture.comparisons() == 1).await;
    assert!(capture.skips().is_empty(), "{:?}", capture.skips());
    assert_eq!(
        metric_value(
            &metrics(&control).await,
            r#"limen_comparisons_total{route="sampled-fast",result="match"}"#
        ),
        Some(1.0)
    );
}

#[tokio::test]
async fn a_sampled_slow_known_length_body_demotes_like_any_other() {
    // The deadline is shape-agnostic: a `Content-Length` body that trickles is
    // exactly as unbufferable as a chunked one, and demotes identically.
    let gate = Gate::new();
    let inner = gate.clone();
    let legacy = raw_upstream(move |mut sock, _head| {
        let gate = inner.clone();
        async move {
            write(
                &mut sock,
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 20\r\nConnection: close\r\n\r\nAAAAAAAAAA",
            )
            .await;
            gate.wait().await;
            write(&mut sock, "BBBBBBBBBB").await;
        }
    })
    .await;
    let new = complete_upstream("AAAAAAAAAABBBBBBBBBB").await;

    let capture = Capture::default();
    let cfg = sampled_config("sampled-sized", &legacy, &new, SHORT_PRIMARY_MS);
    let (data, _control) = planes(&cfg, &capture);

    let resp = get(&data, "/sized").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("content-length").unwrap(), "20");
    capture
        .expect("comparison_skipped:response_buffer_timeout")
        .await;

    let mut chunks = resp.into_body().into_data_stream();
    let first = read_at_least(&mut chunks, 10, "the buffered prefix").await;
    gate.open();
    let mut whole = first;
    whole.extend_from_slice(&drain(chunks).await);
    assert_eq!(whole, b"AAAAAAAAAABBBBBBBBBB");
}

#[tokio::test]
async fn a_demotion_releases_the_shadow_permit_for_the_next_request() {
    // With exactly one shadow slot in the whole process, a demotion that leaked
    // its permit would leave every later request skipped for
    // `concurrency_limit` — the failure mode that turns one slow response into
    // a campaign with no comparisons at all.
    let gate = Gate::new();
    let inner = gate.clone();
    let legacy = raw_upstream(move |mut sock, head| {
        let gate = inner.clone();
        async move {
            write(&mut sock, &chunked_head("text/plain")).await;
            if head.starts_with("GET /slow") {
                write(&mut sock, &chunk("AAAAAAAAAA")).await;
                gate.wait().await;
            }
            write(&mut sock, &chunk("done")).await;
            write(&mut sock, "0\r\n\r\n").await;
        }
    })
    .await;
    let new = complete_upstream("done").await;

    let capture = Capture::default();
    // Two routes over the same pair of upstreams and the same single shadow
    // slot, so only the *budget* differs: `/slow` is meant to demote, `/fast`
    // is meant to compare. Sharing one short budget would put the second half
    // of this test one slow CI runner away from demoting too — and a demoted
    // second request proves nothing about the first one's permit.
    let cfg = config_from_yaml(&format!(
        r#"
server: {{ shadow_concurrency_limit: 1 }}
routes:
  - id: permit-slow
    match: {{ methods: ["GET"], path_prefix: "/slow" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{new}"
    mode: shadow_legacy_primary
    timeouts: {{ primary_ms: {SHORT_PRIMARY_MS}, shadow_ms: 2000 }}
    comparison: {{ enabled: true, sample_rate: 1.0, max_body_bytes: 262144 }}
  - id: permit-fast
    match: {{ methods: ["GET"], path_prefix: "/fast" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{new}"
    mode: shadow_legacy_primary
    timeouts: {{ primary_ms: {GENEROUS_PRIMARY_MS}, shadow_ms: 2000 }}
    comparison: {{ enabled: true, sample_rate: 1.0, max_body_bytes: 262144 }}
"#
    ));
    let (data, _control) = planes(&cfg, &capture);

    let slow = get(&data, "/slow").await;
    assert_eq!(slow.status(), 200);
    capture
        .expect("comparison_skipped:response_buffer_timeout")
        .await;
    gate.open();
    assert!(!drain(slow.into_body().into_data_stream()).await.is_empty());

    let fast = get(&data, "/fast").await;
    assert_eq!(fast.status(), 200);
    assert_eq!(drain(fast.into_body().into_data_stream()).await, b"done");
    common::wait_until("the second request's comparison", || {
        capture.comparisons() == 1
    })
    .await;
    assert!(
        !capture
            .skips()
            .iter()
            .any(|s| s.ends_with("concurrency_limit")),
        "the demoted request kept its permit: {:?}",
        capture.skips()
    );
}

#[tokio::test]
async fn a_fully_stalled_response_still_demotes() {
    // The trap a per-chunk budget check would fall into: after the headers this
    // upstream sends nothing at all and never closes, so there is no next chunk
    // to do the arithmetic on. Only a bound around the await itself fires.
    let gate = Gate::new();
    let inner = gate.clone();
    let legacy = raw_upstream(move |mut sock, _head| {
        let gate = inner.clone();
        async move {
            write(&mut sock, &chunked_head("text/plain")).await;
            // Held open (not closed — a close would end the stream and look
            // like an error) until the test lets the connection go.
            gate.wait().await;
        }
    })
    .await;
    let new = complete_upstream("anything").await;

    let capture = Capture::default();
    let cfg = sampled_config("sampled-stalled", &legacy, &new, SHORT_PRIMARY_MS);
    let (data, _control) = planes(&cfg, &capture);

    let resp = get(&data, "/stalled").await;
    assert_eq!(resp.status(), 200);
    capture
        .expect("comparison_skipped:response_buffer_timeout")
        .await;
    // The body is deliberately not read: it never ends. Releasing the upstream
    // lets its task finish with the test.
    gate.open();
}

#[tokio::test]
async fn an_upstream_that_dies_while_buffering_still_yields_a_synthesized_502() {
    // Death *before* the demotion is unchanged by the deadline: nothing has
    // been committed to the client yet, so limen answers with its own 502
    // rather than a truncated 200.
    let legacy = raw_upstream(move |mut sock, _head| async move {
        write(&mut sock, &chunked_head("text/plain")).await;
        write(&mut sock, &chunk("AAAAAAAAAA")).await;
        // Dropped mid-body: no terminal chunk, so the client sees a broken
        // stream rather than a complete one.
        drop(sock);
    })
    .await;
    let new = complete_upstream("AAAAAAAAAA").await;

    let capture = Capture::default();
    // A budget far longer than the death takes, so this is the death path and
    // not the deadline path.
    let cfg = sampled_config("sampled-death", &legacy, &new, GENEROUS_PRIMARY_MS);
    let (data, _control) = planes(&cfg, &capture);

    let resp = get(&data, "/dies").await;
    assert_eq!(resp.status(), 502);
    assert!(capture.skips().is_empty(), "{:?}", capture.skips());
}
