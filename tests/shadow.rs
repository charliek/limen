//! `shadow_legacy_primary` mode: the client is always served legacy; eligible
//! reads are shadowed to new and compared off the client path. The shadow's
//! latency, failures, and comparison never affect the client.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use common::{config_from_yaml, parts, router_with_observer, send};
use limen::compare::result::ComparisonResult;
use limen::observability::{ShadowFailure, ShadowObserver, SkipReason};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A test observer that records comparison outcomes for assertions.
#[derive(Clone, Default)]
struct Capture {
    comparisons: Arc<Mutex<Vec<ComparisonResult>>>,
    failures: Arc<Mutex<Vec<String>>>,
    skips: Arc<Mutex<Vec<String>>>,
}

impl ShadowObserver for Capture {
    fn shadow_dispatched(&self, _route_id: &str) {}
    fn comparison(&self, _route_id: &str, result: &ComparisonResult) {
        self.comparisons.lock().unwrap().push(result.clone());
    }
    fn shadow_skipped(&self, _route_id: &str, reason: SkipReason) {
        self.skips.lock().unwrap().push(reason.as_str().to_string());
    }
    fn shadow_failed(&self, _route_id: &str, failure: ShadowFailure) {
        self.failures
            .lock()
            .unwrap()
            .push(failure.as_str().to_string());
    }
    fn comparison_skipped(&self, _route_id: &str, reason: SkipReason) {
        self.skips.lock().unwrap().push(reason.as_str().to_string());
    }
}

impl Capture {
    fn comparisons(&self) -> Vec<ComparisonResult> {
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
    let result = capture.comparisons().pop().unwrap();
    assert!(
        result.is_match(),
        "equivalent JSON should match: {result:?}"
    );
    assert!(result.differences.is_empty());
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
    let result = capture.comparisons().pop().unwrap();
    assert!(!result.is_match());
    assert!(
        result.differences.iter().any(|d| d.path == "$.name"),
        "diff should include $.name at sample_rate 1.0: {result:?}"
    );
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
