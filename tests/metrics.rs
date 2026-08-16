//! Metrics-scrape test (spec §10.1, Phase 7 acceptance): driving a request
//! populates the Prometheus exposition with the expected names and *bounded*
//! labels — and never leaks a high-cardinality or secret value (tenant id,
//! request id, raw path) into a label.

mod common;

use std::path::Path;

use axum::body::Body;
use axum::http::Request;
use common::{config_from_yaml, parts, send};
use limen::health::endpoints::ControlState;
use limen::http::server::{build_state, control_plane_router, data_plane_router};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn scrape_exposes_bounded_metrics_without_secret_labels() {
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&legacy)
        .await;

    // A legacy_only route serves the request metric; a breaker-guarded route
    // populates the circuit-breaker state gauge.
    let cfg = config_from_yaml(&format!(
        r#"
routes:
  - id: get-device
    match: {{ methods: ["GET"], path_prefix: "/devices/" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{legacy}"
    mode: failover_to_legacy
    failover_safe: true
    circuit_breaker:
      enabled: true
      failure_rate_threshold: 0.5
      min_requests: 5
      open_duration_ms: 30000
      half_open_max_requests: 2
  - id: list-devices
    match: {{ methods: ["GET"], path_prefix: "/devices" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{legacy}"
    mode: legacy_only
"#,
        legacy = legacy.uri()
    ));

    // Install the recorder and build both planes over one shared state.
    let handle = limen::observability::prometheus::install();
    let state = build_state(&cfg, Path::new(".")).expect("build state");
    let data = data_plane_router(state.clone());
    let control = control_plane_router(
        ControlState::new(
            state.flags().clone(),
            state.routes_arc(),
            handle,
            state.fail_safe_mode(),
        ),
        "/metrics",
    );

    // Drive a request carrying values that must NOT end up in labels.
    let resp = send(
        &data,
        Request::builder()
            .uri("/devices")
            .header("x-request-id", "super-secret-trace-id")
            .header("x-tenant-id", "tenant-acme-42")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let (status, headers, _) = parts(resp).await;
    assert_eq!(status, 200);
    // The request id is echoed back to the client for correlation.
    assert_eq!(
        headers.get("x-request-id").unwrap(),
        "super-secret-trace-id"
    );

    // Scrape /metrics.
    let scrape = send(
        &control,
        Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let (status, _, body) = parts(scrape).await;
    assert_eq!(status, 200);

    // Expected metric families with bounded labels.
    assert!(
        body.contains("limen_requests_total"),
        "request counter present"
    );
    assert!(body.contains(r#"route="list-devices""#), "route id label");
    assert!(body.contains(r#"method="GET""#), "method label");
    assert!(body.contains(r#"upstream="legacy""#), "upstream label");
    assert!(body.contains(r#"status_class="2xx""#), "status class label");
    assert!(
        body.contains("limen_request_duration_seconds"),
        "latency histogram"
    );
    assert!(body.contains("limen_in_flight_requests"), "in-flight gauge");
    assert!(
        body.contains(r#"limen_circuit_breaker_state{route="get-device""#),
        "breaker state gauge per route"
    );
    assert!(
        body.contains("limen_flag_provider_stale"),
        "flag health gauge"
    );

    // Cardinality/secrecy discipline: no tenant id, request id, or raw path in
    // any label.
    assert!(
        !body.contains("tenant-acme-42"),
        "tenant id must never appear in a metric label"
    );
    assert!(
        !body.contains("super-secret-trace-id"),
        "request id must never appear in a metric label"
    );
    assert!(
        !body.contains(r#"path="#),
        "raw paths must never be a metric label"
    );
}

/// The `limen_requests_total` exposition line for a given route, or panic.
fn requests_line<'a>(body: &'a str, route: &str) -> &'a str {
    let needle = format!(r#"route="{route}""#);
    body.lines()
        .find(|l| l.starts_with("limen_requests_total{") && l.contains(&needle))
        .unwrap_or_else(|| panic!("no limen_requests_total series for route {route}\n{body}"))
}

#[tokio::test]
async fn failover_replay_is_counted_as_legacy_served() {
    // When a failover route's new attempt fails and the request is replayed to
    // legacy, the request metric must reflect the upstream that actually served
    // the client (legacy) — not the chosen primary (new).
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("from-legacy"))
        .mount(&legacy)
        .await;

    // New is an unreachable address: the attempt fails (a transport error),
    // triggering the failover replay to legacy.
    let cfg = config_from_yaml(&format!(
        r#"
routes:
  - id: failover-replay
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    new_upstream: "http://127.0.0.1:1"
    mode: failover_to_legacy
    failover_safe: true
    timeouts: {{ primary_ms: 1000, shadow_ms: 1000 }}
"#,
        legacy = legacy.uri()
    ));

    let handle = limen::observability::prometheus::install();
    let state = build_state(&cfg, Path::new(".")).expect("build state");
    let data = data_plane_router(state.clone());
    let control = control_plane_router(
        ControlState::new(
            state.flags().clone(),
            state.routes_arc(),
            handle,
            state.fail_safe_mode(),
        ),
        "/metrics",
    );

    // New 500s, so the request fails over to legacy and the client gets 200.
    let resp = send(
        &data,
        Request::builder().uri("/x").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(parts(resp).await.0, 200);

    let (_, _, body) = parts(
        send(
            &control,
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await,
    )
    .await;

    let line = requests_line(&body, "failover-replay");
    assert!(
        line.contains(r#"upstream="legacy""#),
        "failover replay must be counted as legacy-served, got: {line}"
    );
    assert!(
        line.contains(r#"status_class="2xx""#),
        "client saw legacy's 200, got: {line}"
    );
    // The new-side failure is visible as an upstream error, not a served request.
    assert!(body.contains(r#"limen_upstream_errors_total{route="failover-replay""#));
}
