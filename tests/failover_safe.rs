//! Mid-flight replay semantics gated by `failover_safe` (spec §6.5, safety
//! invariant 4): a `failover_safe` route replays a failed in-flight request to
//! legacy; a route without the flag returns the new-side failure untouched.
//!
//! Two modes qualify. `failover_to_legacy` always sends to new, so the flag has
//! governed its replay from the start. `percentage_split` qualifies for the
//! requests its bucket sent to new (plan 016) — same machinery, same guarantee,
//! just a narrower slice of traffic. `new_only` deliberately does not: it has no
//! legacy leg to replay against.
//!
//! The counts here are the point. Every fixture pins *exact* wiremock call
//! counts on both upstreams, because the failure this file exists to catch is a
//! request executed twice against something that is not idempotent.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request};
use axum::Router;
use bytes::Bytes;
use common::{config_from_yaml, parts, raw_upstream, router, router_with_observer, send};
use limen::compare::result::ComparisonResult;
use limen::config::model::Config;
use limen::observability::{ShadowFailure, ShadowMeta, ShadowObserver, SkipReason};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An address with no listener — connecting fails immediately (a "down" new).
/// Only for fixtures whose assertion does not depend on *counting* new's
/// attempts; where it does, use [`counted_dead_upstream`] instead.
const DEAD_UPSTREAM: &str = "http://127.0.0.1:1";

// --- Fixtures --------------------------------------------------------------

fn config(legacy: &str, failover_safe: bool) -> Config {
    config_from_yaml(&format!(
        r#"
routes:
  - id: r
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{DEAD_UPSTREAM}"
    mode: failover_to_legacy
    failover_safe: {failover_safe}
    timeouts: {{ primary_ms: 1000, shadow_ms: 1000 }}
"#
    ))
}

/// One `percentage_split` route over a static flag, plus the handful of knobs
/// these tests vary. They all come through here so that a difference in outcome
/// is attributable to the knob under test rather than to a hand-copied YAML
/// block that drifted from its siblings.
#[derive(Clone)]
struct Split {
    route: String,
    legacy: String,
    new: String,
    /// The static rollout percentage: 100 puts every key on new, 0 none of them.
    percentage: u32,
    failover_safe: bool,
    /// YAML list body for `match.methods`.
    methods: String,
    /// `server.request_body_limit_bytes`, for the tests about the buffer bound.
    body_limit: Option<u64>,
    /// A breaker's `open_duration_ms`, for the tests that assert on steering.
    breaker_open_ms: Option<u64>,
    /// `timeouts.primary_ms` — the one budget both failover legs share.
    primary_ms: u64,
}

impl Split {
    /// The default fixture: everything on new, replay opted into, `GET`.
    fn new(route: &str, legacy: &str, new: &str) -> Self {
        Self {
            route: route.to_string(),
            legacy: legacy.to_string(),
            new: new.to_string(),
            percentage: 100,
            failover_safe: true,
            methods: r#""GET""#.to_string(),
            body_limit: None,
            breaker_open_ms: None,
            primary_ms: 1000,
        }
    }

    fn build(&self) -> Config {
        let Self {
            route,
            legacy,
            new,
            percentage,
            failover_safe,
            methods,
            primary_ms,
            ..
        } = self;
        let server = match self.body_limit {
            Some(bytes) => format!("server: {{ request_body_limit_bytes: {bytes} }}\n"),
            None => String::new(),
        };
        let breaker = match self.breaker_open_ms {
            Some(open_ms) => format!(
                "    circuit_breaker:\n      \
                 enabled: true\n      \
                 failure_rate_threshold: 0.5\n      \
                 min_requests: 2\n      \
                 open_duration_ms: {open_ms}\n      \
                 half_open_max_requests: 1\n"
            ),
            None => String::new(),
        };
        config_from_yaml(&format!(
            r#"
{server}flags:
  provider: static
  static:
    values:
      "migration.{route}.percentage": {percentage}
  stale_ttl_ms: 30000
  fail_safe_mode: legacy_only
routes:
  - id: {route}
    match: {{ methods: [{methods}], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{new}"
    mode: percentage_split
    failover_safe: {failover_safe}
    timeouts: {{ primary_ms: {primary_ms}, shadow_ms: 1000 }}
    rollout:
      percentage_flag: "migration.{route}.percentage"
      default_percentage: 0
      assignment_key: {{ header: "x-tenant-id", fallback: request_random }}
{breaker}"#
        ))
    }
}

/// A mock answering every `GET`/`POST` with `status` and a self-identifying
/// body, so a fixture can tell which upstream actually served the client.
async fn upstream(name: &str, status: u16) -> MockServer {
    let server = MockServer::start().await;
    for verb in ["GET", "POST"] {
        Mock::given(method(verb))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("x-upstream", name)
                    .set_body_string(format!("from-{name}")),
            )
            .mount(&server)
            .await;
    }
    server
}

/// A "new" upstream that accepts the connection, **counts** the attempt, and
/// then hangs up without answering it — a transport failure the test can count.
/// A closed port cannot be: nothing on the far end tallies anything, so a bug
/// that attempted new twice before replaying would read exactly like a bug-free
/// single attempt. Returns the origin URL and the attempt counter.
async fn counted_dead_upstream() -> (String, Arc<AtomicUsize>) {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    let url = raw_upstream(move |sock, _head| {
        let counter = counter.clone();
        async move {
            // Counted before the hang-up, so the error the client eventually
            // observes can never race ahead of the tally that caused it.
            counter.fetch_add(1, Ordering::SeqCst);
            drop(sock);
        }
    })
    .await;
    (url, attempts)
}

/// Send one `GET /x`; returns the status and the `x-upstream` marker.
async fn get(app: &Router) -> (u16, Option<String>) {
    let resp = send(
        app,
        Request::builder().uri("/x").body(Body::empty()).unwrap(),
    )
    .await;
    let (status, headers, _) = parts(resp).await;
    let served = headers
        .get("x-upstream")
        .map(|v| v.to_str().unwrap().to_string());
    (status.as_u16(), served)
}

/// Send one `POST /x` carrying `body`.
async fn post(app: &Router, body: &str) -> (u16, Option<String>) {
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
    let served = headers
        .get("x-upstream")
        .map(|v| v.to_str().unwrap().to_string());
    (status.as_u16(), served)
}

/// How many requests a mock has received.
async fn calls(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

// --- failover_to_legacy (unchanged behavior) -------------------------------

#[tokio::test]
async fn failover_safe_replays_to_legacy() {
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-upstream", "legacy")
                .set_body_string("from-legacy"),
        )
        .mount(&legacy)
        .await;

    let app = router(&config(&legacy.uri(), true));
    let resp = send(
        &app,
        Request::builder().uri("/x").body(Body::empty()).unwrap(),
    )
    .await;

    // New is unreachable, so the failover_safe route replays to legacy and the
    // client gets legacy's response.
    let (status, headers, body) = parts(resp).await;
    assert_eq!(status, 200);
    assert_eq!(headers.get("x-upstream").unwrap(), "legacy");
    assert_eq!(body, "from-legacy");
    assert_eq!(legacy.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn non_failover_safe_returns_new_failure_without_replay() {
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&legacy)
        .await;

    let app = router(&config(&legacy.uri(), false));
    let resp = send(
        &app,
        Request::builder().uri("/x").body(Body::empty()).unwrap(),
    )
    .await;

    // The new-side failure is returned; the in-flight request is NOT replayed.
    assert_eq!(parts(resp).await.0, 502);
    assert_eq!(
        legacy.received_requests().await.unwrap().len(),
        0,
        "the failed request must not be replayed to legacy"
    );
}

#[tokio::test]
async fn failover_safe_relays_successful_new_response_intact() {
    // On the failover path the new response is buffered (bounded) before being
    // committed, so that a body-level failure can fail over. A normal 2xx with a
    // body must still be relayed to the client intact, and legacy left untouched.
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-upstream", "new")
                .set_body_string("from-new"),
        )
        .mount(&new)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("from-legacy"))
        .mount(&legacy)
        .await;

    let cfg = config_from_yaml(&format!(
        r#"
routes:
  - id: r
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{}"
    new_upstream: "{}"
    mode: failover_to_legacy
    failover_safe: true
    timeouts: {{ primary_ms: 1000, shadow_ms: 1000 }}
"#,
        legacy.uri(),
        new.uri()
    ));
    let app = router(&cfg);
    let resp = send(
        &app,
        Request::builder().uri("/x").body(Body::empty()).unwrap(),
    )
    .await;

    let (status, headers, body) = parts(resp).await;
    assert_eq!(status, 200);
    assert_eq!(headers.get("x-upstream").unwrap(), "new");
    assert_eq!(body, "from-new");
    assert_eq!(
        legacy.received_requests().await.unwrap().len(),
        0,
        "legacy must not be hit when new succeeds"
    );
}

/// The `failover_to_legacy` twin of the split over-limit fixture below: the two
/// modes reach `failover_dispatch` by different gates but must refuse an
/// un-bufferable body identically, and neither may leak the breaker slot it was
/// admitted on.
#[tokio::test]
async fn failover_mode_refuses_an_over_limit_body_before_either_upstream() {
    let (legacy, new) = tokio::join!(upstream("legacy", 200), upstream("new", 500));
    let cfg = config_from_yaml(&format!(
        r#"
server: {{ request_body_limit_bytes: 64 }}
routes:
  - id: r
    match: {{ methods: ["POST"], path_prefix: "/" }}
    legacy_upstream: "{}"
    new_upstream: "{}"
    mode: failover_to_legacy
    failover_safe: true
    timeouts: {{ primary_ms: 1000, shadow_ms: 1000 }}
    circuit_breaker:
      enabled: true
      failure_rate_threshold: 0.5
      min_requests: 2
      open_duration_ms: 100
      half_open_max_requests: 1
"#,
        legacy.uri(),
        new.uri()
    ));
    assert_over_limit_is_refused_without_leaking_a_slot(&router(&cfg), &legacy, &new).await;
}

// --- percentage_split ------------------------------------------------------

/// The load-bearing widening: a split route's new bucket, on a route that has
/// attested idempotence, gets the same client-invisible replay `failover_safe`
/// has always bought a `failover_to_legacy` route.
#[tokio::test]
async fn split_new_bucket_replays_a_transport_failure_to_legacy() {
    let legacy = upstream("legacy", 200).await;
    let (new_url, new_attempts) = counted_dead_upstream().await;
    let app = router(&Split::new("split-transport", &legacy.uri(), &new_url).build());

    assert_eq!(get(&app).await, (200, Some("legacy".into())));
    assert_eq!(
        new_attempts.load(Ordering::SeqCst),
        1,
        "new is attempted once — the replay is a failover, not a retry loop"
    );
    assert_eq!(
        calls(&legacy).await,
        1,
        "exactly one legacy call: replayed once, not once per attempt"
    );
}

/// Without the flag the pre-existing semantics stand, unchanged by the
/// widening: the new-side failure is what the client gets, and legacy never
/// sees the request.
#[tokio::test]
async fn split_new_bucket_without_the_flag_is_never_replayed() {
    let legacy = upstream("legacy", 200).await;
    let (new_url, new_attempts) = counted_dead_upstream().await;
    let app = router(
        &Split {
            failover_safe: false,
            ..Split::new("split-unsafe", &legacy.uri(), &new_url)
        }
        .build(),
    );

    assert_eq!(get(&app).await, (502, None));
    assert_eq!(
        new_attempts.load(Ordering::SeqCst),
        1,
        "new is attempted exactly once"
    );
    assert_eq!(
        calls(&legacy).await,
        0,
        "an unattested route must never replay the in-flight request"
    );
}

/// The other bucket keeps the zero-copy path. `failover_safe` buys the *new*
/// bucket a bounded buffer; it must not quietly impose that buffer — and its
/// `request_body_limit_bytes` ceiling — on traffic that was never going to new.
/// Asserted with a body the buffered path would refuse outright (413): at 0% it
/// streams through untouched.
#[tokio::test]
async fn split_legacy_bucket_streams_an_over_limit_body() {
    let (legacy, new) = tokio::join!(upstream("legacy", 200), upstream("new", 200));
    let app = router(
        &Split {
            percentage: 0,
            methods: r#""POST""#.to_string(),
            body_limit: Some(64),
            ..Split::new("split-stream", &legacy.uri(), &new.uri())
        }
        .build(),
    );

    let big = "x".repeat(4096);
    assert_eq!(post(&app, &big).await, (200, Some("legacy".into())));
    assert_eq!(calls(&new).await, 0, "the legacy bucket never contacts new");

    let received = legacy.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(
        received[0].body.len(),
        big.len(),
        "the whole body reached legacy, so it was streamed, not buffer-bounded"
    );
}

/// A 5xx from new is a new-side failure, exactly as it is in failover mode.
#[tokio::test]
async fn split_new_5xx_is_replayed_to_legacy() {
    let (legacy, new) = tokio::join!(upstream("legacy", 200), upstream("new", 503));
    let app = router(&Split::new("split-5xx", &legacy.uri(), &new.uri()).build());

    assert_eq!(get(&app).await, (200, Some("legacy".into())));
    assert_eq!(calls(&new).await, 1, "new attempted exactly once");
    assert_eq!(calls(&legacy).await, 1, "and replayed exactly once");
}

/// A 4xx is the new upstream *working* — it answered, and it answered about the
/// request. Replaying it would hand the client a second, contradictory opinion
/// and hide a real client-side bug behind legacy's tolerance of it. It is also
/// a breaker success: four 4xx in a row at `min_requests: 2` must leave the
/// circuit closed, so new keeps receiving every one of them.
#[tokio::test]
async fn split_new_4xx_is_returned_and_counts_as_a_breaker_success() {
    let (legacy, new) = tokio::join!(upstream("legacy", 200), upstream("new", 404));
    let app = router(
        &Split {
            breaker_open_ms: Some(60_000),
            ..Split::new("split-4xx", &legacy.uri(), &new.uri())
        }
        .build(),
    );

    for _ in 0..4 {
        assert_eq!(get(&app).await, (404, Some("new".into())));
    }
    assert_eq!(
        calls(&new).await,
        4,
        "4xx is a success, so the breaker never opened"
    );
    assert_eq!(calls(&legacy).await, 0, "and nothing was replayed");
}

/// The subtle one. A replay makes the *client* whole, which must not make the
/// *breaker* believe new is healthy: the health signal is new's own outcome,
/// not the outcome the client saw. Every client here gets a 200, and the
/// breaker still opens after `min_requests` new-side failures and steers the
/// rest — new is contacted exactly twice out of six requests.
#[tokio::test]
async fn a_replayed_success_does_not_hide_the_new_failure_from_the_breaker() {
    let (legacy, new) = tokio::join!(upstream("legacy", 200), upstream("new", 500));
    let app = router(
        &Split {
            breaker_open_ms: Some(60_000),
            ..Split::new("split-breaker", &legacy.uri(), &new.uri())
        }
        .build(),
    );

    for _ in 0..6 {
        assert_eq!(get(&app).await, (200, Some("legacy".into())));
    }
    assert_eq!(
        calls(&new).await,
        2,
        "the breaker opened on the two failures and steered the rest"
    );
    assert_eq!(
        calls(&legacy).await,
        6,
        "two replays plus four steered requests"
    );
}

/// Replay identity: the request legacy sees must be the request new saw. A
/// replay that dropped the query, re-encoded the body, or lost a header would
/// be a *different* request answered under the client's original one.
#[tokio::test]
async fn the_legacy_replay_is_identical_to_the_new_attempt() {
    let (legacy, new) = tokio::join!(upstream("legacy", 200), upstream("new", 500));
    let app = router(
        &Split {
            methods: r#""POST""#.to_string(),
            ..Split::new("split-identity", &legacy.uri(), &new.uri())
        }
        .build(),
    );

    let body = r#"{"order":42,"idempotency_key":"abc"}"#;
    let mut request = Request::builder()
        .method("POST")
        .uri("/orders/42?dry_run=false&trace=on")
        .header("x-custom", "trace-me")
        .header("content-type", "application/json")
        // An id the client supplied, so the expected value is known rather than
        // generated — both legs must carry *this* one.
        .header("x-request-id", "fixture-request-id")
        // A hop recorded by a load balancer in front of limen.
        .header("x-forwarded-for", "198.51.100.9")
        .body(Body::from(body))
        .unwrap();
    // Without a `ConnectInfo` extension limen has no client address, omits
    // `X-Forwarded-For` entirely, and a header-equality check would then be
    // comparing two absences. Injecting one is what makes the generated
    // forwarding headers exist to be compared at all.
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 44444))));
    let resp = send(&app, request).await;
    assert_eq!(parts(resp).await.0, 200);

    let attempted = new.received_requests().await.unwrap();
    let replayed = legacy.received_requests().await.unwrap();
    assert_eq!(attempted.len(), 1, "new attempted once");
    assert_eq!(replayed.len(), 1, "legacy replayed once");
    let (attempted, replayed) = (&attempted[0], &replayed[0]);

    assert_eq!(attempted.method, replayed.method);
    assert_eq!(attempted.url.path(), replayed.url.path());
    assert_eq!(attempted.url.query(), replayed.url.query());
    assert_eq!(attempted.url.query(), Some("dry_run=false&trace=on"));
    assert_eq!(attempted.body, body.as_bytes());
    assert_eq!(attempted.body, replayed.body);
    // Named values, not just equality: two upstreams that both received the
    // wrong thing would compare equal. `X-Forwarded-For` must be the inbound
    // chain extended by limen's view of the client, on both legs.
    for (leg, req) in [("new", attempted), ("legacy", replayed)] {
        for (header, expected) in [
            ("x-custom", "trace-me"),
            ("x-request-id", "fixture-request-id"),
            ("x-forwarded-for", "198.51.100.9, 203.0.113.7"),
            ("x-forwarded-proto", "http"),
        ] {
            assert_eq!(
                req.headers
                    .get(header)
                    .unwrap_or_else(|| panic!("{leg} leg is missing {header}: {:?}", req.headers)),
                expected,
                "{leg} leg carries the wrong {header}"
            );
        }
    }
    // Every forwarded header, not just the interesting ones — `host` excepted,
    // which is per-origin by definition and the one field that *must* differ.
    let forwarded = |req: &wiremock::Request| {
        let mut headers: Vec<(String, String)> = req
            .headers
            .iter()
            .filter(|(name, _)| name.as_str() != "host")
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        headers.sort();
        headers
    };
    assert_eq!(
        forwarded(attempted),
        forwarded(replayed),
        "the replay must carry the same filtered headers new was sent"
    );
}

/// A `POST` on a split route is a write the operator explicitly attested as
/// replay-safe. That attestation is the whole gate, so it has to actually buy
/// the replay — body intact.
#[tokio::test]
async fn split_post_is_replayed_when_the_route_attests_it_is_safe() {
    let legacy = upstream("legacy", 200).await;
    let (new_url, new_attempts) = counted_dead_upstream().await;
    let app = router(
        &Split {
            methods: r#""POST""#.to_string(),
            ..Split::new("split-post", &legacy.uri(), &new_url)
        }
        .build(),
    );

    assert_eq!(
        post(&app, "the-write-payload").await,
        (200, Some("legacy".into()))
    );
    assert_eq!(
        new_attempts.load(Ordering::SeqCst),
        1,
        "the write reached new exactly once before being replayed"
    );
    let received = legacy.received_requests().await.unwrap();
    assert_eq!(received.len(), 1, "replayed once, not twice");
    assert_eq!(received[0].body, b"the-write-payload");
}

/// The buffer bound (invariant 6) applies to the split gate too: a body too
/// large to hold cannot be replayed, and `failover_safe` promised a replay — so
/// the request is refused up front rather than sent un-replayably. Neither
/// upstream is contacted, and the breaker slot the decision reserved is
/// *released*, not recorded: a failure recorded against new here would blame it
/// for a request it never saw, and a leaked half-open slot would wedge the
/// breaker shut forever at `half_open_max_requests: 1`.
#[tokio::test]
async fn split_refuses_an_over_limit_body_before_either_upstream() {
    let (legacy, new) = tokio::join!(upstream("legacy", 200), upstream("new", 500));
    let app = router(
        &Split {
            methods: r#""POST""#.to_string(),
            body_limit: Some(64),
            breaker_open_ms: Some(100),
            ..Split::new("split-too-large", &legacy.uri(), &new.uri())
        }
        .build(),
    );
    assert_over_limit_is_refused_without_leaking_a_slot(&app, &legacy, &new).await;
}

/// A body that errors mid-read is not a body that is too large, and saying so
/// with a 413 would send an operator hunting a size limit that was never
/// reached. The failover path buffers the request body, so it is the one place
/// that has to tell the two apart: over the limit is 413, an aborted or broken
/// upload is the streaming path's own 400.
#[tokio::test]
async fn a_request_body_that_errors_mid_read_is_a_bad_request_not_a_413() {
    let (legacy, new) = tokio::join!(upstream("legacy", 200), upstream("new", 200));
    let app = router(
        &Split {
            methods: r#""POST""#.to_string(),
            body_limit: Some(1_048_576),
            ..Split::new("split-broken-body", &legacy.uri(), &new.uri())
        }
        .build(),
    );

    // A body far *under* the limit that simply stops working part-way through:
    // the client went away, or the upload broke.
    let broken = Body::from_stream(futures::stream::iter(vec![
        Ok(Bytes::from_static(b"the-first-half")),
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

    let (status, _, text) = parts(resp).await;
    assert_eq!(status, 400, "an unreadable body is a client fault, not 413");
    assert!(
        text.contains("could not be read"),
        "and says which fault it is: {text}"
    );
    assert_eq!(
        calls(&new).await,
        0,
        "a body we can't replay never goes out"
    );
    assert_eq!(calls(&legacy).await, 0);
}

/// Both failover legs draw on **one** `primary_ms` budget
/// (`docs/guides/resilience.md`: "one absolute deadline for the whole primary
/// leg"). The positive control: new burning most of the budget and then failing
/// still leaves the replay the remainder, so a slow-but-failing new does not
/// silently cost the client its failover.
#[tokio::test]
async fn the_replay_runs_inside_what_new_left_of_the_budget() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    // New spends ~700ms of a 1000ms budget, then fails; legacy answers in
    // ~150ms, comfortably inside the ~300ms remainder.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_delay(Duration::from_millis(700)))
        .mount(&new)
        .await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-upstream", "legacy")
                .set_delay(Duration::from_millis(150)),
        )
        .mount(&legacy)
        .await;

    let app = router(
        &Split {
            primary_ms: 1000,
            ..Split::new("split-budget-remainder", &legacy.uri(), &new.uri())
        }
        .build(),
    );
    let started = Instant::now();
    assert_eq!(get(&app).await, (200, Some("legacy".into())));
    let elapsed = started.elapsed();

    assert_eq!(calls(&new).await, 1);
    assert_eq!(calls(&legacy).await, 1, "the replay still happened");
    assert!(
        elapsed < Duration::from_millis(1_500),
        "the whole exchange stayed near one budget, took {elapsed:?}"
    );
}

/// The consequence of that one budget, and the case that pins it: a new attempt
/// that **times out** has spent the budget, so there is nothing left to replay
/// in and the client gets the 504. Replaying here would hand the client ~2× the
/// deadline its route declared — the failure mode the single budget exists to
/// prevent. The breaker has still recorded the failure, so *subsequent*
/// requests are steered to legacy; the route converges away from a sick new
/// upstream without doubling anyone's wait to do it.
#[tokio::test]
async fn a_new_timeout_spends_the_budget_and_is_not_replayed() {
    let legacy = upstream("legacy", 200).await;
    let new = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .mount(&new)
        .await;

    let app = router(
        &Split {
            primary_ms: 300,
            ..Split::new("split-budget-spent", &legacy.uri(), &new.uri())
        }
        .build(),
    );
    let started = Instant::now();
    let (status, served) = get(&app).await;
    let elapsed = started.elapsed();

    assert_eq!(status, 504, "the client gets new's timeout");
    assert_eq!(served, None, "and it is limen's own response");
    assert_eq!(calls(&new).await, 1);
    assert_eq!(
        calls(&legacy).await,
        0,
        "a timeout leaves no budget to replay in"
    );
    assert!(
        elapsed < Duration::from_millis(1_000),
        "the client waited one budget, not two, took {elapsed:?}"
    );
}

/// Shared body for the two over-limit fixtures. `app` must route `POST` to a
/// `failover_safe` new upstream that 500s, behind a breaker with
/// `min_requests: 2`, `half_open_max_requests: 1` and a 100ms open window, over
/// a 64-byte `request_body_limit_bytes`.
async fn assert_over_limit_is_refused_without_leaking_a_slot(
    app: &Router,
    legacy: &MockServer,
    new: &MockServer,
) {
    // Two small writes fail on new and are replayed; that opens the breaker.
    for _ in 0..2 {
        assert_eq!(post(app, "small").await, (200, Some("legacy".into())));
    }
    assert_eq!(calls(new).await, 2, "the breaker opens after two failures");
    assert_eq!(calls(legacy).await, 2);

    // Let the open window elapse, so the next request is admitted as the single
    // half-open trial — i.e. it holds the slot when it is refused.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let (status, served) = post(app, &"x".repeat(4096)).await;
    assert_eq!(
        status, 413,
        "an un-bufferable body is refused, not forwarded"
    );
    assert_eq!(served, None, "and refused by limen, not by an upstream");
    assert_eq!(calls(new).await, 2, "the refused request never reached new");
    assert_eq!(calls(legacy).await, 2, "nor legacy");

    // The slot was released rather than consumed: new is still re-testable.
    assert_eq!(post(app, "small").await, (200, Some("legacy".into())));
    assert_eq!(
        calls(new).await,
        3,
        "the half-open slot was released, so new is re-tried"
    );

    // …and *released* is not the same as *recorded as a success*, which would
    // also have let that request through. The two are told apart by what the
    // trial's own failure then does. Released: the trial above was the half-open
    // probe, its 5xx reopens the breaker, and the next request never reaches
    // new. Falsely recorded as a success: the breaker would have closed, that
    // 5xx would be one failure in a fresh window (below `min_requests: 2`, and
    // at the boundary `failure_rate_threshold` is a strict `>`), the breaker
    // would stay closed, and new would be called a fourth time.
    assert_eq!(post(app, "small").await, (200, Some("legacy".into())));
    assert_eq!(
        calls(new).await,
        3,
        "the failed probe reopened the breaker, so the next request is steered \
         past new — the refusal released the slot rather than passing it"
    );
}

/// Regression tripwire for the ordering the widened gate depends on. The
/// failover path returns before `shadow::plan` is ever called, and `plan` is
/// mode-gated to `shadow_legacy_primary` besides — so a split route on this
/// path must produce no shadow, no comparison, and no skip. The observer is the
/// single seam every `limen_shadow_*` / `limen_comparison*` counter is fed
/// through, so counting its callbacks counts those metrics exactly.
#[tokio::test]
async fn the_failover_path_never_shadows_or_compares() {
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<&'static str>>>);
    impl Capture {
        fn note(&self, what: &'static str) {
            self.0.lock().unwrap().push(what);
        }
    }
    impl ShadowObserver for Capture {
        fn shadow_dispatched(&self, _meta: &ShadowMeta) {
            self.note("shadow_dispatched");
        }
        fn comparison(&self, _meta: &ShadowMeta, _result: &ComparisonResult) {
            self.note("comparison");
        }
        fn shadow_skipped(&self, _meta: &ShadowMeta, _reason: SkipReason) {
            self.note("shadow_skipped");
        }
        fn shadow_failed(&self, _meta: &ShadowMeta, _failure: ShadowFailure) {
            self.note("shadow_failed");
        }
        fn comparison_skipped(&self, _meta: &ShadowMeta, _reason: SkipReason) {
            self.note("comparison_skipped");
        }
    }

    let (legacy, new) = tokio::join!(upstream("legacy", 200), upstream("new", 500));
    let cfg = Split::new("split-no-shadow", &legacy.uri(), &new.uri()).build();

    // The deterministic half: ask the planner directly. `shadow::plan` is the
    // sole producer of shadow work, and for this exact route config — split,
    // failover_safe, comparison at its defaults — it returns `None`, so there
    // is nothing for the early return to have raced with. No timing involved.
    let state = limen::http::server::build_state(&cfg, Path::new(".")).expect("build state");
    let route = state
        .routes()
        .match_route("GET", "/x", None)
        .expect("the fixture route matches");
    let url: url::Url = "http://upstream.invalid/x".parse().unwrap();
    assert!(
        limen::http::shadow::plan(
            route,
            &Method::GET,
            &HeaderMap::new(),
            url.clone(),
            &url,
            "req-id",
        )
        .is_none(),
        "a percentage_split route must plan no shadow, whatever the proxy does with it"
    );

    // The integration half: drive a real replay and watch the observer seam.
    let capture = Capture::default();
    let app = router_with_observer(&cfg, Arc::new(capture.clone()));
    assert_eq!(get(&app).await, (200, Some("legacy".into())));

    // A fire-and-forget shadow reports asynchronously, so a single read the
    // instant the client is answered would pass even if one had been spawned.
    // Poll instead, failing on the first callback rather than after a fixed
    // wait: a regression is caught as soon as it reports, and a clean run still
    // gives a spawned task ~500ms of scheduling to betray itself in.
    for _ in 0..50 {
        let seen = capture.0.lock().unwrap().clone();
        assert!(
            seen.is_empty(),
            "the failover path must not reach the comparison surface, saw {seen:?}"
        );
        drop(seen);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
