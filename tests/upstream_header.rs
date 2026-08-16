//! `debug.upstream_header` (plan 016, W3): a debug-gated `x-limen-upstream`
//! response header attributing the upstream whose response is being
//! *relayed* to the client.
//!
//! The pinned contract (not "attempted", not "served" in the loose sense
//! `Dispatched` uses internally):
//! - a normal relay carries the serving upstream's name;
//! - a failover replay that relays legacy's response (whether the route is
//!   `failover_to_legacy` or a `percentage_split` bucket on `failover_safe`)
//!   carries `legacy`, because legacy's response is what the client actually
//!   received;
//! - every limen-synthesized response — a synthesized 502/504, a local
//!   refusal (413 over-limit, 400 unreadable body), an unmatched route —
//!   carries no header at all, on or off;
//! - the header is committed the moment `dispatch` decides to relay, so it
//!   describes limen's decision, not the eventual fate of the body stream: a
//!   response too large (or too slow) to buffer still relays streamed, and a
//!   body that then breaks mid-transfer cannot retroactively change it.
//!
//! Inbound `x-limen-upstream` is always stripped, debug on or off: a client
//! can never make it reach an upstream, and an upstream can never make it
//! reach the client unfiltered.
//!
//! Every attribution assertion also checks `x-marker`, an oracle independent
//! of the header under test — set by each mock/raw upstream itself — so a
//! test can't pass because the header merely *named* the right upstream while
//! the *other* one actually answered.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, Request};
use axum::Router;
use bytes::Bytes;
use common::{config_from_yaml, parts, raw_upstream, router, send, write};
use futures::StreamExt;
use limen::config::model::Config;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An address with no listener — connecting fails immediately, so a route
/// pointed at it always hits a transport-level failure without needing a
/// counted or gated fixture.
const DEAD_UPSTREAM: &str = "http://127.0.0.1:1";

/// A `percentage_split` route over a static flag, plus the handful of knobs
/// these tests vary. They all come through here so that a difference in
/// outcome is attributable to the knob under test rather than to a
/// hand-copied YAML block that drifted from its siblings — mirrors
/// `tests/failover_safe.rs`'s `Split` fixture.
#[derive(Clone)]
struct SplitCase {
    legacy: String,
    new: String,
    /// The static rollout percentage: 0 always buckets legacy, 100 always
    /// buckets new — static so the tests don't need a stable assignment key.
    percentage: u32,
    failover_safe: bool,
    /// `debug.upstream_header`.
    upstream_header: bool,
    /// `server.request_body_limit_bytes`, for the tests about the buffer bound.
    body_limit: Option<u64>,
    /// YAML list body for `match.methods`.
    methods: String,
    /// `timeouts.primary_ms` — a caller-chosen value only for the one test
    /// that needs a short timeout against an upstream that never answers.
    primary_ms: u64,
}

impl SplitCase {
    /// The default fixture: legacy-only, no debug header, `GET`+`POST`,
    /// unbounded body, a 1s primary budget.
    fn new(legacy: &str, new: &str) -> Self {
        Self {
            legacy: legacy.to_string(),
            new: new.to_string(),
            percentage: 0,
            failover_safe: false,
            upstream_header: false,
            body_limit: None,
            methods: r#""GET", "POST""#.to_string(),
            primary_ms: 1000,
        }
    }

    fn build(&self) -> Config {
        let Self {
            legacy,
            new,
            percentage,
            failover_safe,
            upstream_header,
            methods,
            primary_ms,
            ..
        } = self;
        let debug = if *upstream_header {
            "debug:\n  upstream_header: true\n"
        } else {
            ""
        };
        let server = match self.body_limit {
            Some(bytes) => format!("server: {{ request_body_limit_bytes: {bytes} }}\n"),
            None => String::new(),
        };
        config_from_yaml(&format!(
            r#"
{debug}{server}flags:
  provider: static
  static:
    values:
      "migration.r.rollout_percentage": {percentage}
  stale_ttl_ms: 30000
  fail_safe_mode: legacy_only
routes:
  - id: r
    match: {{ methods: [{methods}], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{new}"
    mode: percentage_split
    failover_safe: {failover_safe}
    timeouts: {{ primary_ms: {primary_ms}, shadow_ms: 1000 }}
    rollout:
      percentage_flag: "migration.r.rollout_percentage"
      default_percentage: 0
      assignment_key: {{ header: "x-tenant-id", fallback: request_random }}
"#
        ))
    }
}

/// A mock answering `GET`/`POST` with `status` and an `x-marker` header
/// identifying which upstream answered — independent of `x-limen-upstream`,
/// so a test can tell the two apart.
async fn marker_upstream(name: &str, status: u16) -> MockServer {
    let server = MockServer::start().await;
    for verb in ["GET", "POST"] {
        Mock::given(method(verb))
            .respond_with(ResponseTemplate::new(status).insert_header("x-marker", name))
            .mount(&server)
            .await;
    }
    server
}

/// Accepts the connection and then never answers — for the one test that
/// needs a genuine timeout, as opposed to [`DEAD_UPSTREAM`]'s immediate
/// connection refusal.
async fn never_responds_upstream() -> String {
    raw_upstream(|sock, _head| async move {
        // `sock` must be referenced inside the `async move` block to be
        // captured by it at all — otherwise it drops the moment this closure
        // returns the future (before the future is ever polled), closing the
        // connection instantly instead of holding it open. Binding it to
        // `_held` keeps it alive for as long as `pending()` never resolves.
        let _held = sock;
        std::future::pending::<()>().await;
    })
    .await
}

/// Send `GET /x`, optionally carrying a spoofed `x-limen-upstream` request
/// header. Returns the status, the response's `x-limen-upstream` value, and
/// its `x-marker` value — the independent oracle for which upstream actually
/// answered, since the header under test only proves what limen *claims*.
async fn get_with_optional_spoof(
    app: &Router,
    spoof: Option<&str>,
) -> (u16, Option<String>, Option<String>) {
    let mut builder = Request::builder().method("GET").uri("/x");
    if let Some(v) = spoof {
        builder = builder.header("x-limen-upstream", v);
    }
    let resp = send(app, builder.body(Body::empty()).unwrap()).await;
    let (status, headers, _) = parts(resp).await;
    let header = headers
        .get("x-limen-upstream")
        .map(|v| v.to_str().unwrap().to_string());
    let marker = headers
        .get("x-marker")
        .map(|v| v.to_str().unwrap().to_string());
    (status.as_u16(), header, marker)
}

async fn get(app: &Router) -> (u16, Option<String>, Option<String>) {
    get_with_optional_spoof(app, None).await
}

async fn post(app: &Router, body: &str) -> (u16, Option<String>, Option<String>) {
    let resp = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/x")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    let (status, headers, _) = parts(resp).await;
    let header = headers
        .get("x-limen-upstream")
        .map(|v| v.to_str().unwrap().to_string());
    let marker = headers
        .get("x-marker")
        .map(|v| v.to_str().unwrap().to_string());
    (status.as_u16(), header, marker)
}

// --- 1. absent when the flag is off -----------------------------------------

#[tokio::test]
async fn header_absent_on_a_relay_when_the_flag_is_off() {
    let (legacy, new) = tokio::join!(marker_upstream("legacy", 200), marker_upstream("new", 200));

    let all_legacy = router(&SplitCase::new(&legacy.uri(), &new.uri()).build());
    assert_eq!(
        get(&all_legacy).await,
        (200, None, Some("legacy".to_string()))
    );

    let all_new = router(
        &SplitCase {
            percentage: 100,
            ..SplitCase::new(&legacy.uri(), &new.uri())
        }
        .build(),
    );
    assert_eq!(get(&all_new).await, (200, None, Some("new".to_string())));
}

// --- 2. attribution on a normal relay, flag on ------------------------------

#[tokio::test]
async fn flag_on_attributes_a_legacy_served_relay() {
    let (legacy, new) = tokio::join!(marker_upstream("legacy", 200), marker_upstream("new", 200));
    let app = router(
        &SplitCase {
            upstream_header: true,
            ..SplitCase::new(&legacy.uri(), &new.uri())
        }
        .build(),
    );
    assert_eq!(
        get(&app).await,
        (200, Some("legacy".to_string()), Some("legacy".to_string()))
    );
}

#[tokio::test]
async fn flag_on_attributes_a_new_served_relay() {
    let (legacy, new) = tokio::join!(marker_upstream("legacy", 200), marker_upstream("new", 200));
    let app = router(
        &SplitCase {
            percentage: 100,
            upstream_header: true,
            ..SplitCase::new(&legacy.uri(), &new.uri())
        }
        .build(),
    );
    assert_eq!(
        get(&app).await,
        (200, Some("new".to_string()), Some("new".to_string()))
    );
}

// --- 3. failover replay attributes legacy -----------------------------------

/// A `percentage_split` bucket sent to new, on a `failover_safe` route, whose
/// new-side transport attempt fails: the replayed legacy response is what the
/// client receives, so the header must say `legacy`, not `new` (the upstream
/// `dispatch` merely attempted) and not absent (this response *was* relayed).
#[tokio::test]
async fn failover_replay_of_legacy_is_attributed_legacy() {
    let legacy = marker_upstream("legacy", 200).await;
    let app = router(
        &SplitCase {
            percentage: 100,
            failover_safe: true,
            upstream_header: true,
            ..SplitCase::new(&legacy.uri(), DEAD_UPSTREAM)
        }
        .build(),
    );
    assert_eq!(
        get(&app).await,
        (200, Some("legacy".to_string()), Some("legacy".to_string()))
    );
}

// --- 4. synthesized responses carry no header, flag on ----------------------

/// (a) split + unsafe + dead new: the new-side failure is returned untouched
/// (no replay), so the 502 is limen-synthesized and must carry no header.
#[tokio::test]
async fn synthesized_502_without_replay_carries_no_header() {
    let legacy = marker_upstream("legacy", 200).await;
    let app = router(
        &SplitCase {
            percentage: 100,
            upstream_header: true,
            ..SplitCase::new(&legacy.uri(), DEAD_UPSTREAM)
        }
        .build(),
    );
    assert_eq!(get(&app).await, (502, None, None));
}

/// (b) split + `failover_safe` + an over-limit body: limen refuses locally,
/// before either upstream is contacted, so the 413 must carry no header.
#[tokio::test]
async fn synthesized_413_over_limit_carries_no_header() {
    let (legacy, new) = tokio::join!(marker_upstream("legacy", 200), marker_upstream("new", 200));
    let app = router(
        &SplitCase {
            percentage: 100,
            failover_safe: true,
            upstream_header: true,
            body_limit: Some(64),
            ..SplitCase::new(&legacy.uri(), &new.uri())
        }
        .build(),
    );
    let big = "x".repeat(4096);
    assert_eq!(post(&app, &big).await, (413, None, None));
}

/// (c) a request body that errors mid-read is a local 400, refused before
/// either upstream is contacted — no header, same as the 413 case.
#[tokio::test]
async fn synthesized_400_unreadable_body_carries_no_header() {
    let (legacy, new) = tokio::join!(marker_upstream("legacy", 200), marker_upstream("new", 200));
    let app = router(
        &SplitCase {
            percentage: 100,
            failover_safe: true,
            upstream_header: true,
            body_limit: Some(1_048_576),
            ..SplitCase::new(&legacy.uri(), &new.uri())
        }
        .build(),
    );

    let broken = Body::from_stream(futures::stream::iter(vec![
        Ok(Bytes::from_static(b"half")),
        Err(std::io::Error::other("the client hung up mid-upload")),
    ]));
    let resp = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/x")
            .body(broken)
            .unwrap(),
    )
    .await;
    let (status, headers, _) = parts(resp).await;
    assert_eq!(status, 400);
    assert!(headers.get("x-limen-upstream").is_none());
}

/// (d) a genuine timeout — the upstream accepts the connection and then never
/// answers — is also limen's own synthesized response, not a relay, and must
/// carry no header even with the flag on.
#[tokio::test]
async fn synthesized_504_timeout_carries_no_header() {
    let legacy = marker_upstream("legacy", 200).await;
    let new = never_responds_upstream().await;
    let app = router(
        &SplitCase {
            percentage: 100,
            upstream_header: true,
            methods: r#""GET""#.to_string(),
            primary_ms: 200,
            ..SplitCase::new(&legacy.uri(), &new)
        }
        .build(),
    );

    let (status, header, marker) = get(&app).await;
    assert_eq!(status, 504);
    assert_eq!(header, None);
    assert_eq!(marker, None, "new never answered, so there is no oracle");
}

// --- 5. carve-outs the contract names explicitly ----------------------------

/// A New response whose body exceeds the buffer bound
/// (`server.request_body_limit_bytes`, the same field `failover_dispatch`
/// buffers a relayed response against) still relays — streamed as-is, the
/// failover guarantee degraded to header-level (the `Buffered::TooLarge`
/// branch at `proxy.rs`'s `failover_dispatch`) — and must still be
/// attributed `new`: it *was* relayed, not synthesized, just not fully
/// buffered.
#[tokio::test]
async fn an_over_limit_new_response_on_a_failover_safe_route_still_attributes_new() {
    let legacy = marker_upstream("legacy", 200).await;
    let new = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-marker", "new")
                .set_body_string("x".repeat(4096)),
        )
        .mount(&new)
        .await;
    let app = router(
        &SplitCase {
            percentage: 100,
            failover_safe: true,
            upstream_header: true,
            body_limit: Some(64),
            ..SplitCase::new(&legacy.uri(), &new.uri())
        }
        .build(),
    );

    assert_eq!(
        get(&app).await,
        (200, Some("new".to_string()), Some("new".to_string()))
    );
    assert_eq!(
        legacy.received_requests().await.unwrap().len(),
        0,
        "new answered 2xx, so there is nothing to fail over"
    );
}

/// The attribution header is committed to the response the moment `dispatch`
/// decides to relay — before axum ever streams a body byte to the client. A
/// body that then breaks mid-transfer must not retroactively change it: the
/// header describes limen's routing decision, not whether the body later made
/// it across intact.
#[tokio::test]
async fn attribution_survives_a_post_commit_body_stream_failure() {
    let legacy = marker_upstream("legacy", 200).await;
    let new = raw_upstream(|mut sock, _head| async move {
        write(
            &mut sock,
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 100\r\nConnection: close\r\n\r\n",
        )
        .await;
        write(&mut sock, "short-body-far-under-the-declared-length").await;
        // `sock` drops here, closing the connection well short of the
        // declared 100 bytes — a real body-stream failure, not a slow one.
    })
    .await;
    let app = router(
        &SplitCase {
            percentage: 100,
            upstream_header: true,
            ..SplitCase::new(&legacy.uri(), &new)
        }
        .build(),
    );

    let resp = send(
        &app,
        Request::builder().uri("/x").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("x-limen-upstream"),
        Some(&HeaderValue::from_static("new")),
        "committed before the body ever streams to the client"
    );

    // Prove the body genuinely fails — otherwise this test proves nothing
    // about "post-commit".
    let mut chunks = resp.into_body().into_data_stream();
    let mut saw_error = false;
    let drained = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match chunks.next().await {
                Some(Ok(_)) => continue,
                Some(Err(_)) => {
                    saw_error = true;
                    return;
                }
                None => return,
            }
        }
    })
    .await;
    assert!(drained.is_ok(), "the body must not hang");
    assert!(
        saw_error,
        "the truncated body must surface as a stream error"
    );
}

/// Two spoofed `x-limen-upstream` request lines (not one) must both be
/// stripped — `filter_headers` drops every line named `x-limen-upstream`, not
/// just the first one `HeaderMap::get` would see.
#[tokio::test]
async fn duplicate_spoofed_inbound_headers_are_both_stripped() {
    let (legacy, new) = tokio::join!(marker_upstream("legacy", 200), marker_upstream("new", 200));
    let app = router(
        &SplitCase {
            upstream_header: true,
            ..SplitCase::new(&legacy.uri(), &new.uri())
        }
        .build(),
    );

    let req = Request::builder()
        .method("GET")
        .uri("/x")
        .header("x-limen-upstream", "evil-1")
        .header("x-limen-upstream", "evil-2")
        .body(Body::empty())
        .unwrap();
    let resp = send(&app, req).await;
    let (status, headers, _) = parts(resp).await;
    assert_eq!(status, 200);
    let values: Vec<&str> = headers
        .get_all("x-limen-upstream")
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect();
    assert_eq!(
        values,
        vec!["legacy"],
        "exactly one line, limen's own — neither spoofed line survived"
    );

    let received = legacy.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(
        received[0]
            .headers
            .get_all("x-limen-upstream")
            .iter()
            .count(),
        0,
        "neither spoofed line may reach the upstream"
    );
}

// --- 6. spoof resistance -----------------------------------------------------

/// A client-forged `x-limen-upstream` must never reach the upstream, and must
/// never be echoed back on the client response — with the flag off.
#[tokio::test]
async fn spoofed_inbound_header_is_stripped_before_the_upstream_flag_off() {
    let (legacy, new) = tokio::join!(marker_upstream("legacy", 200), marker_upstream("new", 200));
    let app = router(&SplitCase::new(&legacy.uri(), &new.uri()).build());

    let (status, header, marker) = get_with_optional_spoof(&app, Some("evil")).await;
    assert_eq!(status, 200);
    assert_eq!(header, None, "must never be echoed back");
    assert_eq!(
        marker,
        Some("legacy".to_string()),
        "legacy actually served (percentage 0)"
    );

    let received = legacy.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert!(
        received[0].headers.get("x-limen-upstream").is_none(),
        "must never reach the upstream"
    );
}

/// Same spoof, with the flag on: the client's forged value must not survive to
/// the upstream, and the response must carry limen's own computed value —
/// never the client's.
#[tokio::test]
async fn spoofed_inbound_header_is_stripped_before_the_upstream_flag_on() {
    let (legacy, new) = tokio::join!(marker_upstream("legacy", 200), marker_upstream("new", 200));
    let app = router(
        &SplitCase {
            upstream_header: true,
            ..SplitCase::new(&legacy.uri(), &new.uri())
        }
        .build(),
    );

    let (status, header, marker) = get_with_optional_spoof(&app, Some("evil")).await;
    assert_eq!(status, 200);
    assert_eq!(header, Some("legacy".to_string()));
    assert_eq!(marker, Some("legacy".to_string()));

    let received = legacy.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert!(received[0].headers.get("x-limen-upstream").is_none());
}

/// An upstream that reflects its own `x-limen-upstream` must never leak it to
/// the client: stripped outright with the flag off, replaced with limen's own
/// value with the flag on.
async fn upstream_with_forged_response_header(name: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-limen-upstream", "evil")
                .insert_header("x-marker", name),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_forged_response_header_from_the_upstream_is_stripped_flag_off() {
    let legacy = upstream_with_forged_response_header("legacy").await;
    let new = marker_upstream("new", 200).await;
    let app = router(&SplitCase::new(&legacy.uri(), &new.uri()).build());

    let resp = send(
        &app,
        Request::builder().uri("/x").body(Body::empty()).unwrap(),
    )
    .await;
    let (status, headers, _) = parts(resp).await;
    assert_eq!(status, 200);
    assert!(headers.get("x-limen-upstream").is_none());
    assert_eq!(
        headers.get("x-marker"),
        Some(&HeaderValue::from_static("legacy")),
        "legacy actually served"
    );
}

#[tokio::test]
async fn a_forged_response_header_from_the_upstream_is_replaced_flag_on() {
    let legacy = upstream_with_forged_response_header("legacy").await;
    let new = marker_upstream("new", 200).await;
    let app = router(
        &SplitCase {
            upstream_header: true,
            ..SplitCase::new(&legacy.uri(), &new.uri())
        }
        .build(),
    );

    let resp = send(
        &app,
        Request::builder().uri("/x").body(Body::empty()).unwrap(),
    )
    .await;
    let (status, headers, _) = parts(resp).await;
    assert_eq!(status, 200);
    assert_eq!(
        headers.get("x-limen-upstream"),
        Some(&HeaderValue::from_static("legacy")),
        "must be limen's own computed value, never the upstream's forged one"
    );
    assert_eq!(
        headers.get("x-marker"),
        Some(&HeaderValue::from_static("legacy")),
        "legacy actually served"
    );
}
