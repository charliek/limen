//! Circuit breaker: repeated new-upstream failures open the circuit, and
//! subsequent requests are steered to legacy.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use common::{config_from_yaml, parts, router, send};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn repeated_new_failures_open_circuit_and_route_to_legacy() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    // New always 500s; legacy is healthy.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&new)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).insert_header("x-upstream", "legacy"))
        .mount(&legacy)
        .await;

    // failover_to_legacy (new is primary) with a breaker that opens after 2
    // requests at >50% failure. failover_safe is false, so the in-flight 500 is
    // returned; the breaker steers *subsequent* requests.
    let cfg = config_from_yaml(&format!(
        r#"
routes:
  - id: r
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{}"
    new_upstream: "{}"
    mode: failover_to_legacy
    circuit_breaker:
      enabled: true
      failure_rate_threshold: 0.5
      min_requests: 2
      open_duration_ms: 60000
      half_open_max_requests: 1
"#,
        legacy.uri(),
        new.uri()
    ));
    let app = router(&cfg);

    let mut last_status = 0u16;
    for _ in 0..6 {
        let resp = send(
            &app,
            Request::builder().uri("/x").body(Body::empty()).unwrap(),
        )
        .await;
        last_status = parts(resp).await.0.as_u16();
    }

    // New stopped receiving once the breaker opened (the first 2 failures).
    assert_eq!(
        new.received_requests().await.unwrap().len(),
        2,
        "new should stop receiving traffic once the breaker opens"
    );
    // Subsequent requests were steered to legacy.
    assert!(legacy.received_requests().await.unwrap().len() >= 4);
    assert_eq!(
        last_status, 200,
        "with the circuit open, the client is served by legacy"
    );
}

#[tokio::test]
async fn locally_rejected_request_does_not_leak_a_half_open_slot() {
    // Regression: a request the breaker admits (reserving the only half-open
    // trial slot) but that is then rejected locally — here a percent-encoded
    // dot-segment path that cannot be forwarded unchanged — must release the
    // slot. Otherwise, with half_open_max_requests=1, the breaker wedges and
    // never re-tests new, even after recovery.
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&new)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
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
    circuit_breaker:
      enabled: true
      failure_rate_threshold: 0.5
      min_requests: 2
      open_duration_ms: 50
      half_open_max_requests: 1
"#,
        legacy.uri(),
        new.uri()
    ));
    let app = router(&cfg);

    // Two new-side 500s open the breaker.
    for _ in 0..2 {
        send(
            &app,
            Request::builder().uri("/x").body(Body::empty()).unwrap(),
        )
        .await;
    }
    let new_after_open = new.received_requests().await.unwrap().len();
    assert_eq!(new_after_open, 2, "breaker opens after 2 failures");

    // Let the open window elapse so the next admitted request is a half-open
    // trial.
    tokio::time::sleep(Duration::from_millis(80)).await;

    // A dot-segment path is admitted by the breaker (reserving the only
    // half-open slot) but rejected with 400 before any upstream call.
    let bad = send(
        &app,
        Request::builder()
            .uri("/a/%2e%2e/b")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(parts(bad).await.0, 400);
    assert_eq!(
        new.received_requests().await.unwrap().len(),
        new_after_open,
        "the rejected request must not reach new",
    );

    // The slot must have been released: a valid request can still trial new.
    send(
        &app,
        Request::builder().uri("/x").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(
        new.received_requests().await.unwrap().len(),
        new_after_open + 1,
        "the half-open slot was released, so new is re-tested",
    );
}
