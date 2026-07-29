//! `X-Forwarded-For`/`X-Forwarded-Proto`/`X-Limen-Shadow` injection (spec
//! §3.6): both headers on primary *and* shadow upstream requests, the shadow
//! marker on the shadow only, an existing `X-Forwarded-For` appended (not
//! replaced), an existing `X-Forwarded-Proto` preserved untouched, and none of
//! the three ever leaking onto the client-facing response.
//!
//! Tests drive the router directly via `tower::oneshot` (see `common`), which
//! has no real accepted connection — so the client address is unknown unless a
//! test inserts an `axum::extract::ConnectInfo` extension itself, mirroring how
//! `http::forwarded::apply` documents the production/test split.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::Request;
use common::{config_from_yaml, parts, router, send};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn test_client_addr() -> SocketAddr {
    "203.0.113.9:54321".parse().unwrap()
}

/// A bare GET to `uri` with a `ConnectInfo` extension set, as production
/// serving would populate it (`Router::into_make_service_with_connect_info`).
fn request_with_known_addr(uri: &str) -> Request<Body> {
    let mut req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    req.extensions_mut().insert(ConnectInfo(test_client_addr()));
    req
}

fn legacy_only_config(legacy: &str) -> limen::config::model::Config {
    config_from_yaml(&format!(
        r#"
routes:
  - id: r
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    mode: legacy_only
"#
    ))
}

fn shadow_config(legacy: &str, new: &str) -> limen::config::model::Config {
    config_from_yaml(&format!(
        r#"
routes:
  - id: r
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{new}"
    mode: shadow_legacy_primary
    timeouts: {{ primary_ms: 2000, shadow_ms: 2000 }}
    comparison: {{ enabled: true, sample_rate: 1.0, max_body_bytes: 262144 }}
"#
    ))
}

/// Poll a mock server's received-request count up to ~2s — the shadow request
/// is dispatched from a detached fire-and-forget task, so it may not have
/// landed the instant the client response returns.
async fn wait_until_received(server: &MockServer, want: usize) {
    for _ in 0..200 {
        if server.received_requests().await.unwrap().len() >= want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected at least {want} received request(s) within timeout");
}

#[tokio::test]
async fn primary_carries_forwarded_for_and_proto_but_never_the_shadow_marker() {
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&legacy)
        .await;

    let app = router(&legacy_only_config(&legacy.uri()));
    let resp = send(&app, request_with_known_addr("/x")).await;
    let (status, client_headers, _body) = parts(resp).await;
    assert_eq!(status, 200);

    let received = legacy.received_requests().await.unwrap();
    let upstream_headers = &received[0].headers;
    assert_eq!(
        upstream_headers.get("x-forwarded-for").unwrap(),
        "203.0.113.9"
    );
    assert_eq!(upstream_headers.get("x-forwarded-proto").unwrap(), "http");
    assert!(
        upstream_headers.get("x-limen-shadow").is_none(),
        "the primary request must never carry the shadow marker"
    );

    // Response-leg check: none of the three headers Limen sets on the
    // *upstream request* are ever copied onto the client-facing response.
    assert!(client_headers.get("x-forwarded-for").is_none());
    assert!(client_headers.get("x-forwarded-proto").is_none());
    assert!(client_headers.get("x-limen-shadow").is_none());
}

#[tokio::test]
async fn forwarded_for_is_omitted_when_the_client_address_is_unknown() {
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&legacy)
        .await;

    let app = router(&legacy_only_config(&legacy.uri()));
    // No `ConnectInfo` extension — the common case for every other test in
    // this suite, which drives the router with no real accepted connection.
    let resp = send(
        &app,
        Request::builder().uri("/x").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(resp.status(), 200);

    let received = legacy.received_requests().await.unwrap();
    let upstream_headers = &received[0].headers;
    assert!(
        upstream_headers.get("x-forwarded-for").is_none(),
        "must omit the header rather than fabricate a client address"
    );
    // X-Forwarded-Proto never depends on the client address, so it is set
    // regardless.
    assert_eq!(upstream_headers.get("x-forwarded-proto").unwrap(), "http");
}

#[tokio::test]
async fn existing_forwarded_for_is_appended_not_replaced() {
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&legacy)
        .await;

    let app = router(&legacy_only_config(&legacy.uri()));
    let mut req = request_with_known_addr("/x");
    req.headers_mut()
        .insert("x-forwarded-for", "198.51.100.1".parse().unwrap());
    let resp = send(&app, req).await;
    assert_eq!(resp.status(), 200);

    let received = legacy.received_requests().await.unwrap();
    assert_eq!(
        received[0].headers.get("x-forwarded-for").unwrap(),
        "198.51.100.1, 203.0.113.9",
        "the client's hop must be appended, not overwrite the load balancer's"
    );
}

#[tokio::test]
async fn existing_forwarded_proto_is_preserved_untouched() {
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&legacy)
        .await;

    let app = router(&legacy_only_config(&legacy.uri()));
    let mut req = request_with_known_addr("/x");
    req.headers_mut()
        .insert("x-forwarded-proto", "https".parse().unwrap());
    let resp = send(&app, req).await;
    assert_eq!(resp.status(), 200);

    let received = legacy.received_requests().await.unwrap();
    assert_eq!(
        received[0].headers.get("x-forwarded-proto").unwrap(),
        "https",
        "a value from a proxy upstream of Limen is authoritative"
    );
}

#[tokio::test]
async fn shadow_request_carries_all_three_headers() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&legacy)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&new)
        .await;

    let app = router(&shadow_config(&legacy.uri(), &new.uri()));
    let resp = send(&app, request_with_known_addr("/x")).await;
    assert_eq!(resp.status(), 200);

    wait_until_received(&new, 1).await;
    let received = new.received_requests().await.unwrap();
    let shadow_headers = &received[0].headers;
    assert_eq!(
        shadow_headers.get("x-forwarded-for").unwrap(),
        "203.0.113.9"
    );
    assert_eq!(shadow_headers.get("x-forwarded-proto").unwrap(), "http");
    assert_eq!(shadow_headers.get("x-limen-shadow").unwrap(), "1");

    // And the primary (legacy) request in the same exchange still never gets
    // the shadow marker.
    let primary_received = legacy.received_requests().await.unwrap();
    assert!(primary_received[0].headers.get("x-limen-shadow").is_none());
}

#[tokio::test]
async fn every_existing_forwarded_for_line_is_preserved_and_appended() {
    // `HeaderMap` can carry the same header name as more than one field line
    // (distinct from one line with commas); a client (or intermediary ahead
    // of Limen) that sent two `X-Forwarded-For` lines must not lose either
    // when Limen appends its own hop.
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&legacy)
        .await;

    let app = router(&legacy_only_config(&legacy.uri()));
    let mut req = request_with_known_addr("/x");
    req.headers_mut()
        .insert("x-forwarded-for", "198.51.100.1".parse().unwrap());
    req.headers_mut()
        .append("x-forwarded-for", "198.51.100.2".parse().unwrap());
    let resp = send(&app, req).await;
    assert_eq!(resp.status(), 200);

    let received = legacy.received_requests().await.unwrap();
    assert_eq!(
        received[0].headers.get("x-forwarded-for").unwrap(),
        "198.51.100.1, 198.51.100.2, 203.0.113.9",
        "neither pre-existing hop may be dropped"
    );
}

#[tokio::test]
async fn forwarded_for_renders_an_ipv6_client_address_without_brackets() {
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&legacy)
        .await;

    let app = router(&legacy_only_config(&legacy.uri()));
    let mut req = Request::builder().uri("/x").body(Body::empty()).unwrap();
    let v6_addr: SocketAddr = "[2001:db8::1]:54321".parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(v6_addr));
    let resp = send(&app, req).await;
    assert_eq!(resp.status(), 200);

    let received = legacy.received_requests().await.unwrap();
    assert_eq!(
        received[0].headers.get("x-forwarded-for").unwrap(),
        "2001:db8::1",
        "XFF carries a bare IP — no port, no brackets"
    );
}

#[tokio::test]
async fn a_client_forged_shadow_marker_never_reaches_the_upstream() {
    // A client sending its own `X-Limen-Shadow: 1` must not be able to make
    // the real upstream believe the primary request is a shadow copy.
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&legacy)
        .await;

    let app = router(&legacy_only_config(&legacy.uri()));
    let mut req = request_with_known_addr("/x");
    req.headers_mut()
        .insert("x-limen-shadow", "1".parse().unwrap());
    let resp = send(&app, req).await;
    assert_eq!(resp.status(), 200);

    let received = legacy.received_requests().await.unwrap();
    assert!(
        received[0].headers.get("x-limen-shadow").is_none(),
        "a client-supplied shadow marker must be stripped, not forwarded"
    );
}

#[tokio::test]
async fn upstream_supplied_forwarded_and_shadow_headers_never_reach_the_client() {
    // Even if an upstream's response happens to carry these header names
    // (e.g. an upstream that reflects request headers), the proxy must not
    // relay them onto the client-facing response.
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-forwarded-for", "10.0.0.1")
                .insert_header("x-forwarded-proto", "https")
                .insert_header("x-limen-shadow", "1"),
        )
        .mount(&legacy)
        .await;

    let app = router(&legacy_only_config(&legacy.uri()));
    let resp = send(&app, request_with_known_addr("/x")).await;
    let (status, client_headers, _body) = parts(resp).await;
    assert_eq!(status, 200);

    assert!(client_headers.get("x-forwarded-for").is_none());
    assert!(client_headers.get("x-forwarded-proto").is_none());
    assert!(client_headers.get("x-limen-shadow").is_none());
}
