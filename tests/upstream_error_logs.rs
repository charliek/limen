//! Safety invariant 5 on the transport-failure paths: a failed upstream request
//! must not put the client's query string (an `access_token`, a signed-URL
//! signature) or an upstream's userinfo into any log line or any client-visible
//! response. reqwest renders a failed request as `... for url (<full URL>)`, so
//! a raw `%error` does exactly that (limen#41).
//!
//! Each test sends a request whose URL carries canary values, makes the
//! upstream fail at the transport level, and asserts the canaries appear in
//! nothing limen wrote — the log captured at every level and target, and the
//! response the client got. Each also proves the canaries *could* have leaked:
//! the same request, failed through a bare reqwest client, renders them. A test
//! that cannot fail proves nothing.

mod common;

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use common::{
    config_from_yaml, parts, router, router_with_observer, send, shadow_config, wait_until,
    LogCapture,
};
use limen::config::model::Config;
use limen::observability::MetricsObserver;
use std::sync::Arc;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const QUERY_CANARY: &str = "CANARY-QUERY-9f3a71";
const USER_CANARY: &str = "CANARY-USER-9f3a71";
const PASS_CANARY: &str = "CANARY-PASS-9f3a71";

/// Nothing listens on port 1, so connecting is refused at once.
fn dead_upstream() -> String {
    format!("http://{USER_CANARY}:{PASS_CANARY}@127.0.0.1:1")
}

/// The path-and-query a client would send: the canary rides a query value limen
/// has no business repeating anywhere.
fn request_uri() -> String {
    format!("/x?access_token={QUERY_CANARY}&page=2")
}

fn get() -> Request<Body> {
    Request::builder()
        .uri(request_uri())
        .body(Body::empty())
        .unwrap()
}

/// Fail to every canary's text, anywhere in `haystack`.
fn assert_no_canary(what: &str, haystack: &str) {
    for canary in [QUERY_CANARY, USER_CANARY, PASS_CANARY] {
        assert!(
            !haystack.contains(canary),
            "{canary} reached {what}:\n{haystack}"
        );
    }
}

/// The control: the very same request, failed through a bare reqwest client and
/// rendered the way a raw `%error` would. Without this the assertions above are
/// vacuous — the canaries might never have been in an error in the first place.
async fn assert_reqwest_would_have_leaked() {
    let error = reqwest::Client::new()
        .get(format!("{}{}", dead_upstream(), request_uri()))
        .send()
        .await
        .expect_err("nothing listens on port 1");
    let rendered = error.to_string();
    assert!(
        rendered.contains(QUERY_CANARY),
        "control: reqwest's own error text should carry the query string, got {rendered}"
    );
}

/// What a request whose every upstream failed at the transport level leaves
/// behind: a bare 502 for the client, and a `marker` log line that says what
/// failed. No canary in either, nor in any other line logged.
async fn assert_failed_without_leak(resp: Response<Body>, logs: &LogCapture, marker: &str) {
    let (status, headers, body) = parts(resp).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body, "limen: upstream request failed\n");
    assert_no_canary("a response header", &format!("{headers:?}"));
    assert_no_canary("the response body", &body);

    let text = logs.text();
    // The event was captured, with the error in it: the absence below is about
    // a line that exists.
    let line = text
        .lines()
        .find(|l| l.contains(marker))
        .unwrap_or_else(|| panic!("{marker:?} was not logged at all:\n{text}"));
    assert!(
        line.contains("error sending request"),
        "the logged error should say what failed: {line}"
    );
    assert_no_canary("the log", &text);
}

fn legacy_only(legacy: &str) -> Config {
    config_from_yaml(&format!(
        r#"
routes:
  - id: r
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    mode: legacy_only
    timeouts: {{ primary_ms: 2000, shadow_ms: 1000 }}
"#
    ))
}

#[tokio::test]
async fn a_failed_primary_request_logs_no_url_and_answers_no_url() {
    assert_reqwest_would_have_leaked().await;
    let logs = LogCapture::start();

    let resp = send(&router(&legacy_only(&dead_upstream())), get()).await;
    assert_failed_without_leak(resp, &logs, "upstream request failed").await;
}

#[tokio::test]
async fn a_failed_failover_logs_no_url_on_either_leg() {
    assert_reqwest_would_have_leaked().await;
    let logs = LogCapture::start();

    // Both legs dead: new fails, the replay to legacy fails too, which is the
    // path that logs "legacy failover also failed".
    let cfg = config_from_yaml(&format!(
        r#"
routes:
  - id: r
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{}"
    new_upstream: "{}"
    mode: failover_to_legacy
    failover_safe: true
    timeouts: {{ primary_ms: 4000, shadow_ms: 1000 }}
"#,
        dead_upstream(),
        dead_upstream(),
    ));
    let resp = send(&router(&cfg), get()).await;
    assert_failed_without_leak(resp, &logs, "legacy failover also failed").await;
}

#[tokio::test]
async fn a_failed_shadow_logs_no_url() {
    assert_reqwest_would_have_leaked().await;
    let logs = LogCapture::start();

    // Legacy answers; the shadow to new is refused. The shadow path never
    // renders the error (it classifies it), and this keeps it that way.
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("from-legacy"))
        .mount(&legacy)
        .await;
    let cfg = shadow_config(&legacy.uri(), &dead_upstream(), 1000);
    let app = router_with_observer(&cfg, Arc::new(MetricsObserver::new()));
    let resp = send(&app, get()).await;
    let (status, _, body) = parts(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "from-legacy");

    // The shadow runs off the client path; `limen.shadow_failed` is its
    // observer's record that it ended in a failure.
    wait_until("the shadow to fail", || {
        logs.text().contains("limen.shadow_failed")
    })
    .await;
    assert_no_canary("the log", &logs.text());
}
