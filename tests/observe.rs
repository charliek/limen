//! Observe mode end to end: the seam in `handle`, the control-plane profile
//! endpoint, and the zero-registered counter (plan 012 §D2/§D3).
//!
//! The seam records the **final client-facing response**, so the load-bearing
//! tests here are the ones that exercise the paths a seam inside `dispatch`
//! never sees: a `legacy_only` route that can never shadow, a `failover_safe`
//! route that returns from `failover_dispatch`, and a request limen refuses
//! locally before contacting any upstream. Each of those serves a real client
//! response, and an unprofiled one would read `observations: 0` forever —
//! indistinguishable from a quiet route, which is precisely the absence≠zero
//! confusion observe mode exists to prevent.

mod common;

use std::path::Path;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use axum::Router;
use common::{config_from_yaml, metric_value, parts, raw_upstream, send, write, Gate};
use futures::StreamExt;
use limen::config::model::Config;
use limen::health::endpoints::ControlState;
use limen::http::server::{build_state, control_plane_router, data_plane_router};
use serde_json::Value;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build both planes over one shared state, wiring the observe recorder into
/// the control plane exactly as `serve_with_shutdown` does.
///
/// `prometheus::install()` hands back one process-wide recorder, and the
/// tests in this file run in parallel against it. Every test here must
/// therefore use route ids nobody else in this file uses — otherwise the
/// exact per-route counter assertions in
/// `the_observation_counter_is_zero_registered_per_route` become
/// intermittently flaky, since a route id shared across two concurrently
/// running tests would let one test's traffic land on the counter another
/// test is asserting an exact value for.
fn planes(cfg: &Config) -> (Router, Router) {
    let handle = limen::observability::prometheus::install();
    let state = build_state(cfg, Path::new(".")).expect("build state");
    let data = data_plane_router(state.clone());
    let mut control = ControlState::new(state.flags().clone(), state.routes_arc(), handle);
    if let Some(recorder) = state.observe_recorder() {
        control = control.with_observe(recorder.clone());
    }
    (data, control_plane_router(control, "/metrics"))
}

async fn get(app: &Router, uri: &str) -> (axum::http::StatusCode, String) {
    let resp = send(
        app,
        Request::builder().uri(uri).body(Body::empty()).unwrap(),
    )
    .await;
    let (status, _, body) = parts(resp).await;
    (status, body)
}

/// The whole profile document, as parsed JSON.
async fn profile(control: &Router) -> Value {
    let (status, body) = get(control, "/observe/profile").await;
    assert_eq!(status, 200, "profile endpoint: {body}");
    serde_json::from_str(&body).expect("profile is JSON")
}

/// A single-route config over `legacy`, with observe mode on (`extra` appends
/// to the observe block, e.g. a sample rate).
fn config(id: &str, legacy: &str, observe: &str) -> Config {
    config_from_yaml(&format!(
        r#"
observe: {observe}
routes:
  - id: {id}
    match: {{ methods: ["GET", "POST"], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    mode: legacy_only
"#
    ))
}

#[tokio::test]
async fn profile_is_zero_filled_for_every_configured_route_before_any_traffic() {
    let legacy = MockServer::start().await;
    let cfg = config_from_yaml(&format!(
        r#"
observe: {{}}
routes:
  - id: zero-a
    match: {{ methods: ["GET"], path_prefix: "/a" }}
    legacy_upstream: "{legacy}"
    mode: legacy_only
  - id: zero-b
    match: {{ methods: ["GET"], path_prefix: "/b" }}
    legacy_upstream: "{legacy}"
    mode: legacy_only
"#,
        legacy = legacy.uri()
    ));
    let (_data, control) = planes(&cfg);

    let profile = profile(&control).await;
    // Absence ≠ zero: both configured routes render, and nothing else does.
    assert_eq!(profile["routes"]["zero-a"]["observations"], 0);
    assert_eq!(profile["routes"]["zero-b"]["observations"], 0);
    assert_eq!(profile["routes"].as_object().unwrap().len(), 2);
}

#[tokio::test]
async fn the_profile_endpoint_does_not_exist_without_the_observe_block() {
    let legacy = MockServer::start().await;
    let cfg = config_from_yaml(&format!(
        r#"
routes:
  - id: unobserved
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    mode: legacy_only
"#,
        legacy = legacy.uri()
    ));
    let (_data, control) = planes(&cfg);
    assert_eq!(get(&control, "/observe/profile").await.0, 404);
}

#[tokio::test]
async fn legacy_only_route_populates_its_profile() {
    let legacy = MockServer::start().await;
    // A megabyte — four times the default `comparison.max_body_bytes`, so a
    // seam that ever moved onto the buffer-for-compare path would be visible.
    let big = vec![b'x'; 1_048_576];
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json; charset=utf-8")
                .set_body_bytes(big.clone()),
        )
        .mount(&legacy)
        .await;

    // `legacy_only` with no `new_upstream` at all: `shadow::plan` can never
    // return `Some`, so nothing downstream of `primary_succeeded`'s first early
    // return ever runs.
    let cfg = config("passive", &legacy.uri(), "{}");
    let (data, control) = planes(&cfg);

    let (status, body) = get(&data, "/things/1?page=2").await;
    assert_eq!(status, 200);
    assert_eq!(body.len(), big.len(), "the body relayed whole");

    let route = &profile(&control).await["routes"]["passive"];
    assert_eq!(route["observations"], 1);
    assert_eq!(route["reads"], 1);
    assert_eq!(route["writes"], 0);
    assert_eq!(route["transport_errors"], 0);
    assert_eq!(route["methods"]["GET"], 1);
    assert_eq!(route["status_classes"]["2xx"], 1);
    assert_eq!(route["distinct_read_paths"], 1);
    assert_eq!(route["content_types"][0], "application/json");
    assert_eq!(route["query_names"][0], "page");
}

/// A raw-TCP upstream that answers with a 20-byte `Content-Length` body,
/// writing the first ten bytes and then **blocking** on `gate` before the last
/// ten — a response body held half-written, which no `wiremock` template can
/// do and which proving the proxy streams requires.
async fn half_written_upstream(gate: Gate) -> String {
    raw_upstream(move |mut sock, _head| {
        let gate = gate.clone();
        async move {
            write(
                &mut sock,
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 20\r\n\r\nAAAAAAAAAA",
            )
            .await;
            gate.wait().await;
            write(&mut sock, "BBBBBBBBBB").await;
        }
    })
    .await
}

#[tokio::test]
async fn the_client_gets_the_first_chunk_before_the_upstream_sends_the_last() {
    let gate = Gate::new();
    let upstream = half_written_upstream(gate.clone()).await;
    let cfg = config("streaming", &upstream, "{}");
    let (data, control) = planes(&cfg);

    // Bounded because a proxy that buffered the response would never return
    // headers at all — it would park inside `dispatch` until `release` fires,
    // and the test must fail on that rather than hang.
    let resp = tokio::time::timeout(
        Duration::from_secs(5),
        send(
            &data,
            Request::builder()
                .uri("/stream?page=2")
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .expect("the response head must arrive before the upstream finishes the body");
    assert_eq!(resp.status(), 200);

    // The load-bearing assertion, and the one a "the body arrived intact" test
    // could not make: the first chunk reaches the client while the upstream
    // still owes the last one. A buffering proxy would park here until the gate
    // opens — which happens only *after* this await returns.
    let mut chunks = resp.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_secs(5), chunks.next())
        .await
        .expect("the first chunk must arrive before the upstream sends the last")
        .expect("a chunk")
        .expect("chunk read");
    assert_eq!(&first[..], b"AAAAAAAAAA");

    // The seam ran on the response headers while the body was still in flight,
    // so the profile is already populated — observation never waits on a body.
    let route = &profile(&control).await["routes"]["streaming"];
    assert_eq!(route["observations"], 1);
    assert_eq!(route["content_types"][0], "text/plain");
    assert_eq!(route["query_names"][0], "page");

    gate.open();
    let mut rest = Vec::new();
    while let Some(chunk) = chunks.next().await {
        rest.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(rest, b"BBBBBBBBBB", "the rest of the body still arrives");
}

#[tokio::test]
async fn a_failover_safe_replay_is_observed() {
    // `failover_dispatch` returns its own client response and never reaches
    // `dispatch`'s primary arms. Before the seam moved to `handle`, a hot
    // failover_safe route read `observations: 0` forever — a served response
    // that looked exactly like no traffic at all.
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("from-legacy"))
        .mount(&legacy)
        .await;

    let cfg = config_from_yaml(&format!(
        r#"
observe: {{}}
routes:
  - id: failing-over
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    new_upstream: "http://127.0.0.1:1"
    mode: failover_to_legacy
    failover_safe: true
    timeouts: {{ primary_ms: 1000, shadow_ms: 1000 }}
"#,
        legacy = legacy.uri()
    ));
    let (data, control) = planes(&cfg);
    let (status, body) = get(&data, "/replayed?page=1").await;
    assert_eq!(status, 200);
    assert_eq!(body, "from-legacy");

    let route = &profile(&control).await["routes"]["failing-over"];
    assert_eq!(route["observations"], 1);
    assert_eq!(route["reads"], 1);
    assert_eq!(route["status_classes"]["2xx"], 1);
    // Legacy's own content type, not limen's — the replayed response is the
    // route's, so its shape is fair game to profile.
    assert_eq!(route["content_types"][0], "text/plain");
    assert_eq!(route["query_names"][0], "page");
    // New never answered, but legacy did — the response the client got is a
    // real upstream response, so this is not a transport error.
    assert_eq!(route["transport_errors"], 0);
}

#[tokio::test]
async fn a_locally_refused_request_is_observed_without_being_called_a_failure() {
    // `build_upstream_url` refuses a path it cannot forward byte-for-byte and
    // limen answers 400 itself. The client got a response on this route, so the
    // route is not quiet — but nothing failed upstream, and nothing about
    // limen's own error page describes what the route serves.
    let legacy = MockServer::start().await;
    let cfg = config("refusing", &legacy.uri(), "{}");
    let (data, control) = planes(&cfg);
    assert_eq!(get(&data, "/a/../admin").await.0, 400);

    let route = &profile(&control).await["routes"]["refusing"];
    assert_eq!(route["observations"], 1);
    assert_eq!(route["reads"], 1);
    assert_eq!(route["status_classes"]["4xx"], 1);
    assert_eq!(route["transport_errors"], 0);
    assert_eq!(
        route["content_types"].as_array().unwrap().len(),
        0,
        "limen's own error page is not the route's content type"
    );
    assert_eq!(legacy.received_requests().await.unwrap().len(), 0);
}

#[tokio::test]
async fn read_metadata_is_recorded_without_a_query_value_ever_appearing() {
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(303)
                .insert_header("location", "/next")
                .insert_header("set-cookie", "session=abc; Path=/")
                .insert_header("content-type", "text/html; charset=utf-8"),
        )
        .mount(&legacy)
        .await;

    let cfg = config("flow-hop", &legacy.uri(), "{}");
    let (data, control) = planes(&cfg);
    assert_eq!(get(&data, "/hop?login_challenge=s3cret-token").await.0, 303);

    let document = profile(&control).await;
    let route = &document["routes"]["flow-hop"];
    assert_eq!(route["redirect_reads"], 1);
    assert_eq!(route["location_reads"], 1);
    assert_eq!(route["set_cookie_reads"], 1);
    assert_eq!(route["status_classes"]["3xx"], 1);
    assert_eq!(route["content_types"][0], "text/html");
    assert_eq!(route["query_names"][0], "login_challenge");

    // Invariant 5 on the profile as an output surface: names, never values —
    // and never a path, a header value, or a cookie.
    let rendered = document.to_string();
    for secret in ["s3cret-token", "session=abc", "/hop", "/next"] {
        assert!(
            !rendered.contains(secret),
            "the profile leaked {secret:?}: {rendered}"
        );
    }
}

#[tokio::test]
async fn repeated_reads_record_stability_and_writes_do_not() {
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("stable"))
        .mount(&legacy)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("stable"))
        .mount(&legacy)
        .await;

    let cfg = config("stability", &legacy.uri(), "{}");
    let (data, control) = planes(&cfg);
    for _ in 0..3 {
        assert_eq!(get(&data, "/same").await.0, 200);
    }
    let resp = send(
        &data,
        Request::builder()
            .method("POST")
            .uri("/same")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), 200);

    let route = &profile(&control).await["routes"]["stability"];
    assert_eq!(route["reads"], 3);
    assert_eq!(route["writes"], 1);
    // Two repeats of one fingerprint, all the same length — and the write
    // contributed none of it.
    assert_eq!(route["length_repeats"], 2);
    assert_eq!(route["length_varied"], 0);
    assert_eq!(route["length_missing"], 0);
    assert_eq!(route["fingerprint_overflow"], false);
    assert_eq!(route["methods"]["POST"], 1);
}

#[tokio::test]
async fn a_transport_failure_is_observed_as_a_transport_error() {
    // Nothing listens on port 1, so the primary send fails outright.
    let cfg = config("dead-upstream", "http://127.0.0.1:1", "{}");
    let (data, control) = planes(&cfg);
    for _ in 0..3 {
        assert_eq!(get(&data, "/anything").await.0, 502);
    }

    let route = &profile(&control).await["routes"]["dead-upstream"];
    assert_eq!(route["observations"], 3);
    assert_eq!(route["reads"], 3);
    assert_eq!(route["transport_errors"], 3);
    // The class of what the client was served, so the profile reconciles with
    // `limen_requests_total` instead of quietly omitting failures.
    assert_eq!(route["status_classes"]["5xx"], 3);
    // The response was limen's, not the route's. Its plain-text body is a fixed
    // length, so a seam that read it would report a perfectly *stable* route —
    // the direction the classifier must never be wrong in.
    assert_eq!(route["length_repeats"], 0);
    assert_eq!(route["length_missing"], 0);
    assert_eq!(route["content_types"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn sampling_at_zero_observes_nothing() {
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&legacy)
        .await;

    let cfg = config("unsampled", &legacy.uri(), "{ sample_rate: 0.0 }");
    let (data, control) = planes(&cfg);
    for _ in 0..5 {
        assert_eq!(get(&data, "/x").await.0, 200);
    }
    let route = &profile(&control).await["routes"]["unsampled"];
    assert_eq!(
        route["observations"], 0,
        "sampled out, but still enumerated"
    );
    assert_eq!(route["reads"], 0);
}

#[tokio::test]
async fn the_observation_counter_is_zero_registered_per_route() {
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&legacy)
        .await;

    let cfg = config_from_yaml(&format!(
        r#"
observe: {{}}
routes:
  - id: counted-hit
    match: {{ methods: ["GET"], path_prefix: "/hit" }}
    legacy_upstream: "{legacy}"
    mode: legacy_only
  - id: counted-quiet
    match: {{ methods: ["GET"], path_prefix: "/quiet" }}
    legacy_upstream: "{legacy}"
    mode: legacy_only
"#,
        legacy = legacy.uri()
    ));
    let (data, control) = planes(&cfg);
    assert_eq!(get(&data, "/hit").await.0, 200);

    let (status, rendered) = get(&control, "/metrics").await;
    assert_eq!(status, 200);
    assert_eq!(
        metric_value(
            &rendered,
            r#"limen_observe_observations_total{route="counted-hit"}"#
        ),
        Some(1.0)
    );
    // The route nobody hit renders zero rather than nothing: "the observer saw
    // nothing here" must not look like "this binary has no observe
    // instrumentation".
    assert_eq!(
        metric_value(
            &rendered,
            r#"limen_observe_observations_total{route="counted-quiet"}"#
        ),
        Some(0.0)
    );
    // Cardinality doctrine: the route id is the only label.
    assert!(
        !rendered.contains(r#"limen_observe_observations_total{route="counted-hit",path"#),
        "no path label may appear: {rendered}"
    );
}
