//! `shadow_legacy_primary` mode: the client is always served legacy; eligible
//! reads are shadowed to new and compared off the client path. The shadow's
//! latency, failures, and comparison never affect the client.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request};
use common::{config_from_yaml, parts, router, router_with_observer, send};
use limen::compare::result::ComparisonResult;
use limen::observability::{
    Fanout, ShadowFailure, ShadowMeta, ShadowObserver, SinkObserver, SkipReason,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A test observer that records comparison outcomes (with their [`ShadowMeta`])
/// for assertions.
///
/// Note on span coverage: the shadow task is wrapped in an
/// `info_span!("shadow", %request_id, route = %route_id)` (see
/// `src/http/shadow.rs::spawn`), so every log line inside it carries these ids.
/// A log-capture harness to assert the span's *fields* directly would be a lot
/// of scaffolding for little signal; instead, `shadow_meta_carries_ids` below
/// asserts the same identifiers reach the observer via `ShadowMeta` — the span
/// and the observer are populated from the same `ShadowRequest` fields
/// (`shadow.rs::ShadowRequest::meta`), so this is equivalent coverage.
#[derive(Clone, Default)]
struct Capture {
    comparisons: Arc<Mutex<Vec<(ShadowMeta, ComparisonResult)>>>,
    failures: Arc<Mutex<Vec<String>>>,
    skips: Arc<Mutex<Vec<String>>>,
}

impl ShadowObserver for Capture {
    fn shadow_dispatched(&self, _meta: &ShadowMeta) {}
    fn comparison(&self, meta: &ShadowMeta, result: &ComparisonResult) {
        self.comparisons
            .lock()
            .unwrap()
            .push((meta.clone(), result.clone()));
    }
    fn shadow_skipped(&self, _meta: &ShadowMeta, reason: SkipReason) {
        self.skips.lock().unwrap().push(reason.as_str().to_string());
    }
    fn shadow_failed(&self, _meta: &ShadowMeta, failure: ShadowFailure) {
        self.failures
            .lock()
            .unwrap()
            .push(failure.as_str().to_string());
    }
    fn comparison_skipped(&self, _meta: &ShadowMeta, reason: SkipReason) {
        self.skips.lock().unwrap().push(reason.as_str().to_string());
    }
}

impl Capture {
    fn comparisons(&self) -> Vec<(ShadowMeta, ComparisonResult)> {
        self.comparisons.lock().unwrap().clone()
    }
    fn failures(&self) -> Vec<String> {
        self.failures.lock().unwrap().clone()
    }
    fn skips(&self) -> Vec<String> {
        self.skips.lock().unwrap().clone()
    }
}

/// Poll `cond` until true, up to ~2s, so a fire-and-forget shadow task can run.
async fn wait_until(cond: impl Fn() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not met within timeout");
}

fn shadow_config(legacy: &str, new: &str, shadow_ms: u64) -> limen::config::model::Config {
    config_from_yaml(&format!(
        r#"
routes:
  - id: r
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{new}"
    mode: shadow_legacy_primary
    timeouts: {{ primary_ms: 2000, shadow_ms: {shadow_ms} }}
    comparison: {{ enabled: true, sample_rate: 1.0, max_body_bytes: 262144 }}
"#
    ))
}

#[tokio::test]
async fn shadow_match_client_gets_legacy_both_hit_no_diff() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    // Equivalent JSON in a different key order — must compare equal.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"a":1,"b":2}"#))
        .mount(&legacy)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"b":2,"a":1}"#))
        .mount(&new)
        .await;

    let capture = Capture::default();
    let app = router_with_observer(
        &shadow_config(&legacy.uri(), &new.uri(), 2000),
        Arc::new(capture.clone()),
    );

    let resp = send(
        &app,
        Request::builder()
            .uri("/devices/1")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let (status, _, body) = parts(resp).await;
    assert_eq!(status, 200);
    assert_eq!(body, r#"{"a":1,"b":2}"#, "client is served the legacy body");

    wait_until(|| !capture.comparisons().is_empty()).await;
    let (meta, result) = capture.comparisons().pop().unwrap();
    assert!(
        result.is_match(),
        "equivalent JSON should match: {result:?}"
    );
    assert!(result.differences.is_empty());
    assert_eq!(meta.route_id, "r");
    assert_eq!(
        new.received_requests().await.unwrap().len(),
        1,
        "new was shadowed"
    );
}

#[tokio::test]
async fn shadow_mismatch_records_diff_client_unaffected() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/devices/1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"name":"A"}"#))
        .mount(&legacy)
        .await;
    Mock::given(method("GET"))
        .and(path("/devices/1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"name":"B"}"#))
        .mount(&new)
        .await;

    let capture = Capture::default();
    let app = router_with_observer(
        &shadow_config(&legacy.uri(), &new.uri(), 2000),
        Arc::new(capture.clone()),
    );

    let resp = send(
        &app,
        Request::builder()
            .uri("/devices/1")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let (status, _, body) = parts(resp).await;
    assert_eq!(status, 200);
    assert_eq!(body, r#"{"name":"A"}"#, "client always gets legacy");

    wait_until(|| !capture.comparisons().is_empty()).await;
    let (_meta, result) = capture.comparisons().pop().unwrap();
    assert!(!result.is_match());
    assert!(
        result.differences.iter().any(|d| d.path == "$.name"),
        "diff should include $.name at sample_rate 1.0: {result:?}"
    );
}

/// The observer receives the originating request's own `x-request-id` (not a
/// freshly generated one) along with the matched route and the concrete
/// method/path — proving `ShadowMeta` is threaded through, not re-derived.
#[tokio::test]
async fn shadow_meta_carries_request_id_route_method_and_path() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/devices/42"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&legacy)
        .await;
    Mock::given(method("GET"))
        .and(path("/devices/42"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&new)
        .await;

    let capture = Capture::default();
    let app = router_with_observer(
        &shadow_config(&legacy.uri(), &new.uri(), 2000),
        Arc::new(capture.clone()),
    );

    let resp = send(
        &app,
        Request::builder()
            .uri("/devices/42")
            .header("x-request-id", "client-sent-id-789")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let (status, _, _) = parts(resp).await;
    assert_eq!(status, 200);

    wait_until(|| !capture.comparisons().is_empty()).await;
    let (meta, _result) = capture.comparisons().pop().unwrap();
    assert_eq!(
        meta.request_id, "client-sent-id-789",
        "the observer must see the client's own request id, not a fresh one"
    );
    assert_eq!(meta.route_id, "r");
    assert_eq!(meta.method, Method::GET);
    assert_eq!(meta.path, "/devices/42");
}

/// The `diff_sink` config block wires a [`limen::observability::SinkObserver`]
/// in *alongside* the production metrics observer (`build_state`'s fan-out), so
/// a real mismatch through the router lands on disk with the request's own id.
#[tokio::test]
async fn diff_sink_persists_a_mismatch_with_the_request_id() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/devices/7"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"name":"A"}"#))
        .mount(&legacy)
        .await;
    Mock::given(method("GET"))
        .and(path("/devices/7"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"name":"B"}"#))
        .mount(&new)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let sink_dir = dir.path().join("diffs");
    let cfg = config_from_yaml(&format!(
        r#"
diff_sink:
  dir: "{}"
routes:
  - id: r
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{}"
    new_upstream: "{}"
    mode: shadow_legacy_primary
    comparison: {{ enabled: true, sample_rate: 1.0, max_body_bytes: 262144 }}
"#,
        sink_dir.display(),
        legacy.uri(),
        new.uri()
    ));
    // The production builder (not the test observer hook), so this exercises
    // the real fan-out wiring.
    let app = router(&cfg);

    let resp = send(
        &app,
        Request::builder()
            .uri("/devices/7")
            .header("x-request-id", "sink-req-7")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let (status, _, body) = parts(resp).await;
    assert_eq!(status, 200);
    assert_eq!(body, r#"{"name":"A"}"#, "client is unaffected by the sink");

    // The sink writes from the detached shadow task, so poll for the file.
    let read_sink = || -> Option<String> {
        let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(&sink_dir)
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        entries.sort();
        let first = entries.first()?;
        std::fs::read_to_string(first)
            .ok()
            .filter(|s| !s.is_empty())
    };
    wait_until(|| read_sink().is_some()).await;

    let contents = read_sink().unwrap();
    assert_eq!(contents.lines().count(), 1);
    let record: serde_json::Value = serde_json::from_str(contents.lines().next().unwrap()).unwrap();
    assert_eq!(record["route_id"], "r");
    assert_eq!(record["request_id"], "sink-req-7");
    assert_eq!(record["method"], "GET");
    assert_eq!(record["path"], "/devices/7");
    assert_eq!(record["body_match"], false);
    assert_eq!(record["mismatch_kinds"], serde_json::json!(["body"]));
    assert_eq!(record["differences"][0]["path"], "$.name");

    // And the report reads it back, keyed by route.
    let report = limen::observability::sink::read_report(
        &sink_dir,
        &limen::observability::sink::ReportFilter::default(),
        3,
    )
    .unwrap();
    assert_eq!(report.total, 1);
    assert_eq!(report.routes[0].route_id, "r");
    assert_eq!(report.routes[0].examples[0].request_id, "sink-req-7");
}

/// A matching comparison through the same wiring leaves the sink directory
/// untouched — the sink is a mismatch archive, not a request log.
#[tokio::test]
async fn diff_sink_writes_nothing_when_the_responses_match() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    for server in [&legacy, &new] {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"name":"A"}"#))
            .mount(server)
            .await;
    }

    let dir = tempfile::tempdir().unwrap();
    let sink_dir = dir.path().join("diffs");
    // Sink first, capture second: when the capture has seen the comparison, the
    // sink has already had its turn — so the assertion below is deterministic
    // rather than a race against a sleep.
    let capture = Capture::default();
    let observer = Fanout::new(vec![
        Arc::new(SinkObserver::new(&sink_dir)),
        Arc::new(capture.clone()),
    ]);
    let app = router_with_observer(
        &shadow_config(&legacy.uri(), &new.uri(), 2000),
        Arc::new(observer),
    );

    let resp = send(
        &app,
        Request::builder()
            .uri("/devices/1")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), 200);

    wait_until(|| !capture.comparisons().is_empty()).await;
    assert!(capture.comparisons()[0].1.is_match());
    assert!(!sink_dir.exists(), "a match must not create the sink dir");
}

#[tokio::test]
async fn shadow_timeout_does_not_affect_client() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .mount(&legacy)
        .await;
    // New is far slower than the 100ms shadow timeout.
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"ok":true}"#)
                .set_delay(Duration::from_millis(1500)),
        )
        .mount(&new)
        .await;

    let capture = Capture::default();
    let app = router_with_observer(
        &shadow_config(&legacy.uri(), &new.uri(), 100),
        Arc::new(capture.clone()),
    );

    let started = std::time::Instant::now();
    let resp = send(
        &app,
        Request::builder().uri("/x").body(Body::empty()).unwrap(),
    )
    .await;
    let elapsed = started.elapsed();
    let (status, _, body) = parts(resp).await;

    assert_eq!(status, 200);
    assert_eq!(body, r#"{"ok":true}"#);
    // The client is served from legacy and is not held up by the slow shadow.
    assert!(
        elapsed < Duration::from_millis(1000),
        "client should not wait on the shadow ({elapsed:?})"
    );

    wait_until(|| !capture.failures().is_empty()).await;
    assert_eq!(capture.failures(), vec!["timeout"]);
}

#[tokio::test]
async fn oversized_primary_serves_full_body_and_skips_comparison() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    let big = "x".repeat(5000);
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(&big))
        .mount(&legacy)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&new)
        .await;

    // max_body_bytes far below the 5000-byte legacy body.
    let cfg = config_from_yaml(&format!(
        r#"
routes:
  - id: r
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{}"
    new_upstream: "{}"
    mode: shadow_legacy_primary
    comparison: {{ enabled: true, sample_rate: 1.0, max_body_bytes: 64 }}
"#,
        legacy.uri(),
        new.uri()
    ));
    let capture = Capture::default();
    let app = router_with_observer(&cfg, Arc::new(capture.clone()));

    let resp = send(
        &app,
        Request::builder().uri("/x").body(Body::empty()).unwrap(),
    )
    .await;
    let (status, _, body) = parts(resp).await;

    // The client still receives the complete (unbuffered) body...
    assert_eq!(status, 200);
    assert_eq!(
        body.len(),
        5000,
        "full body must be served despite the limit"
    );
    // ...and the comparison is skipped (so the new upstream is not shadowed).
    wait_until(|| !capture.skips().is_empty()).await;
    assert_eq!(capture.skips(), vec!["response_too_large"]);
    assert!(capture.comparisons().is_empty());
    assert_eq!(new.received_requests().await.unwrap().len(), 0);
}

/// A route may opt a write method into shadowing (`comparison.shadow_methods`).
/// The buffered request body must reach **both** upstreams byte-identically,
/// framed consistently (a `Content-Length` matching the bytes, no
/// `Transfer-Encoding`), and the comparison must run as it does for reads.
#[tokio::test]
async fn opted_in_post_replays_an_identical_body_to_both_upstreams() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/devices"))
        .respond_with(ResponseTemplate::new(201).set_body_string(r#"{"id":1}"#))
        .mount(&legacy)
        .await;
    Mock::given(method("POST"))
        .and(path("/devices"))
        .respond_with(ResponseTemplate::new(201).set_body_string(r#"{"id":1}"#))
        .mount(&new)
        .await;

    let body = r#"{"name":"probe","tags":["a","b"]}"#;
    let cfg = config_from_yaml(&format!(
        r#"
routes:
  - id: r
    match: {{ methods: ["GET", "POST"], path_prefix: "/" }}
    legacy_upstream: "{}"
    new_upstream: "{}"
    mode: shadow_legacy_primary
    comparison:
      enabled: true
      sample_rate: 1.0
      max_body_bytes: 262144
      shadow_methods: ["POST"]
"#,
        legacy.uri(),
        new.uri()
    ));
    let capture = Capture::default();
    let app = router_with_observer(&cfg, Arc::new(capture.clone()));

    let resp = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/devices")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    let (status, _, client_body) = parts(resp).await;
    assert_eq!(status, 201);
    assert_eq!(client_body, r#"{"id":1}"#, "client is served legacy");

    wait_until(|| !capture.comparisons().is_empty()).await;
    let (meta, result) = capture.comparisons().pop().unwrap();
    assert!(result.is_match(), "{result:?}");
    assert_eq!(meta.method, Method::POST);

    let legacy_reqs = legacy.received_requests().await.unwrap();
    let new_reqs = new.received_requests().await.unwrap();
    assert_eq!(new_reqs.len(), 1, "the opted-in write was shadowed");
    assert_eq!(
        new_reqs[0].body, legacy_reqs[0].body,
        "shadow and primary must receive identical bytes"
    );
    assert_eq!(new_reqs[0].body, body.as_bytes());
    for req in [&legacy_reqs[0], &new_reqs[0]] {
        assert_eq!(
            req.headers.get("content-length").unwrap(),
            body.len().to_string().as_str(),
            "framing must match the replayed bytes"
        );
        assert!(req.headers.get("transfer-encoding").is_none());
        assert_eq!(req.headers.get("content-type").unwrap(), "application/json");
    }
    // The shadow copy is still marked as such; the primary never is.
    assert_eq!(new_reqs[0].headers.get("x-limen-shadow").unwrap(), "1");
    assert!(legacy_reqs[0].headers.get("x-limen-shadow").is_none());
}

/// An opted-in write whose body exceeds `max_body_bytes` is not shadowed at all
/// (invariant 6): the comparison is skipped, and the primary still receives the
/// complete body — streamed, never fully buffered — with the client served from
/// it as usual.
#[tokio::test]
async fn oversized_post_body_skips_the_shadow_and_streams_to_the_primary() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(202).set_body_string("accepted"))
        .mount(&legacy)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&new)
        .await;

    let big = "x".repeat(5000);
    let cfg = config_from_yaml(&format!(
        r#"
routes:
  - id: r
    match: {{ methods: ["POST"], path_prefix: "/" }}
    legacy_upstream: "{}"
    new_upstream: "{}"
    mode: shadow_legacy_primary
    comparison:
      enabled: true
      sample_rate: 1.0
      max_body_bytes: 64
      shadow_methods: ["POST"]
"#,
        legacy.uri(),
        new.uri()
    ));
    let capture = Capture::default();
    let app = router_with_observer(&cfg, Arc::new(capture.clone()));

    let resp = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/devices")
            .body(Body::from(big.clone()))
            .unwrap(),
    )
    .await;
    let (status, _, client_body) = parts(resp).await;
    assert_eq!(status, 202);
    assert_eq!(client_body, "accepted", "the client gets the primary reply");

    let legacy_reqs = legacy.received_requests().await.unwrap();
    assert_eq!(legacy_reqs.len(), 1);
    assert_eq!(
        legacy_reqs[0].body.len(),
        5000,
        "the primary must still receive the whole body"
    );
    assert_eq!(capture.skips(), vec!["request_too_large"]);
    assert!(capture.comparisons().is_empty());
    assert_eq!(
        new.received_requests().await.unwrap().len(),
        0,
        "an unbufferable body is never shadowed"
    );
}

/// When the shadow concurrency limit is already saturated, an opted-in write is
/// skipped *before* its body is buffered — the limiter exists to shed work under
/// load, so the client must not pay buffering latency for a shadow that would be
/// refused anyway. The primary still receives the body, streamed.
#[tokio::test]
async fn a_saturated_limiter_skips_an_opted_in_post_without_buffering() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&legacy)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(202).set_body_string("accepted"))
        .mount(&legacy)
        .await;
    // The GET shadow occupies the single slot for the rest of the test.
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("{}")
                .set_delay(Duration::from_secs(30)),
        )
        .mount(&new)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&new)
        .await;

    let cfg = config_from_yaml(&format!(
        r#"
server:
  shadow_concurrency_limit: 1
routes:
  - id: r
    match: {{ methods: ["GET", "POST"], path_prefix: "/" }}
    legacy_upstream: "{}"
    new_upstream: "{}"
    mode: shadow_legacy_primary
    timeouts: {{ primary_ms: 2000, shadow_ms: 60000 }}
    comparison:
      enabled: true
      sample_rate: 1.0
      max_body_bytes: 262144
      shadow_methods: ["POST"]
"#,
        legacy.uri(),
        new.uri()
    ));
    let capture = Capture::default();
    let app = router_with_observer(&cfg, Arc::new(capture.clone()));

    // The permit is taken before the client response is built, so once this
    // returns the limiter is deterministically saturated by the hanging shadow.
    let resp = send(
        &app,
        Request::builder().uri("/read").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    // Let the detached shadow actually reach `new`, so the request-count
    // assertion below is about routing rather than about task scheduling.
    for _ in 0..200 {
        if !new.received_requests().await.unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(new.received_requests().await.unwrap().len(), 1);

    let started = std::time::Instant::now();
    let resp = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/devices")
            .body(Body::from(r#"{"name":"probe"}"#))
            .unwrap(),
    )
    .await;
    let elapsed = started.elapsed();
    let (status, _, body) = parts(resp).await;
    assert_eq!(status, 202);
    assert_eq!(body, "accepted");
    assert!(
        elapsed < Duration::from_secs(5),
        "the client must not wait on the saturated shadow ({elapsed:?})"
    );

    assert_eq!(capture.skips(), vec!["concurrency_limit"]);
    assert!(capture.comparisons().is_empty());
    let legacy_reqs = legacy.received_requests().await.unwrap();
    assert_eq!(legacy_reqs.len(), 2, "the primary saw both requests");
    assert_eq!(legacy_reqs[1].body, br#"{"name":"probe"}"#);
    let new_reqs = new.received_requests().await.unwrap();
    assert_eq!(
        new_reqs.len(),
        1,
        "only the in-flight GET shadow reached new"
    );
    assert_eq!(new_reqs[0].method.as_str(), "GET");
}

/// The opt-in relaxes the body gate for the listed write method only: a
/// body-bearing read is still not shadowed, because the shadow replays a read
/// bodyless and could not reproduce that body faithfully.
#[tokio::test]
async fn a_body_bearing_get_is_still_not_shadowed_on_an_opted_in_route() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    for server in [&legacy, &new] {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(server)
            .await;
    }

    let cfg = config_from_yaml(&format!(
        r#"
routes:
  - id: r
    match: {{ methods: ["GET", "POST"], path_prefix: "/" }}
    legacy_upstream: "{}"
    new_upstream: "{}"
    mode: shadow_legacy_primary
    comparison:
      enabled: true
      sample_rate: 1.0
      max_body_bytes: 262144
      shadow_methods: ["POST"]
"#,
        legacy.uri(),
        new.uri()
    ));
    let capture = Capture::default();
    let app = router_with_observer(&cfg, Arc::new(capture.clone()));

    let resp = send(
        &app,
        Request::builder()
            .method("GET")
            .uri("/devices")
            .header("content-length", "2")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), 200);

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(new.received_requests().await.unwrap().len(), 0);
    assert!(capture.comparisons().is_empty());
    assert!(capture.skips().is_empty(), "not a skip — simply ineligible");
}

/// The default (no `shadow_methods`) is unchanged: a write on a shadowing route
/// is never shadowed — safety invariant 3.
#[tokio::test]
async fn writes_are_never_shadowed() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(201).set_body_string("created"))
        .mount(&legacy)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&new)
        .await;

    // A shadow route that also matches POST: the write must NOT be shadowed.
    let cfg = config_from_yaml(&format!(
        r#"
routes:
  - id: r
    match: {{ methods: ["GET", "POST"], path_prefix: "/" }}
    legacy_upstream: "{}"
    new_upstream: "{}"
    mode: shadow_legacy_primary
    comparison: {{ enabled: true, sample_rate: 1.0, max_body_bytes: 262144 }}
"#,
        legacy.uri(),
        new.uri()
    ));
    let capture = Capture::default();
    let app = router_with_observer(&cfg, Arc::new(capture.clone()));

    let resp = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/devices")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), 201);

    // Give any (erroneous) shadow a chance to fire, then assert none did.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        new.received_requests().await.unwrap().len(),
        0,
        "writes are never shadowed"
    );
    assert_eq!(legacy.received_requests().await.unwrap().len(), 1);
    assert!(capture.comparisons().is_empty());
}
