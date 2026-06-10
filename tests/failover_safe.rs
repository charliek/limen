//! `failover_to_legacy` replay semantics gated by `failover_safe` (spec §6.5):
//! a failover_safe route replays a failed in-flight request to legacy; a
//! non-failover_safe route returns the new-side failure (no replay).

mod common;

use axum::body::Body;
use axum::http::Request;
use common::{config_from_yaml, parts, router, send};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An address with no listener — connecting fails immediately (a "down" new).
const DEAD_UPSTREAM: &str = "http://127.0.0.1:1";

fn config(legacy: &str, failover_safe: bool) -> limen::config::model::Config {
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
