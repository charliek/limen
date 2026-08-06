//! Cross-cutting proxy behavior: path-traversal refusal, hop-by-hop / Connection
//! header stripping, response content-length preservation, and query-aware route
//! selection.

mod common;

use axum::body::Body;
use axum::http::Request;
use common::{config_from_yaml, parts, router, send};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn legacy_only_config(legacy: &str) -> limen::config::model::Config {
    config_from_yaml(&format!(
        r#"
routes:
  - id: r
    match: {{ methods: ["GET"], path_prefix: "/devices/" }}
    legacy_upstream: "{legacy}"
    mode: legacy_only
"#
    ))
}

#[tokio::test]
async fn refuses_dot_segment_path_with_400() {
    let legacy = MockServer::start().await;
    // Catch-all: if the proxy ever forwarded, this would record a request.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&legacy)
        .await;

    let app = router(&legacy_only_config(&legacy.uri()));
    let resp = send(
        &app,
        Request::builder()
            .method("GET")
            .uri("/devices/%2e%2e/admin")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(resp.status(), 400);
    // The traversal request must never reach the upstream.
    assert_eq!(legacy.received_requests().await.unwrap().len(), 0);
}

#[tokio::test]
async fn strips_connection_named_and_hop_by_hop_headers() {
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/devices/1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&legacy)
        .await;

    let app = router(&legacy_only_config(&legacy.uri()));
    let resp = send(
        &app,
        Request::builder()
            .method("GET")
            .uri("/devices/1")
            .header("connection", "keep-alive, x-secret")
            .header("x-secret", "leak")
            .header("x-keep", "kept")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), 200);

    let received = legacy.received_requests().await.unwrap();
    let headers = &received[0].headers;
    assert!(
        headers.get("x-secret").is_none(),
        "Connection-named header must be stripped"
    );
    assert!(
        headers.get("connection").is_none(),
        "hop-by-hop header must be stripped"
    );
    assert!(
        headers.get("x-keep").is_some(),
        "ordinary headers are preserved"
    );
}

#[tokio::test]
async fn preserves_response_content_length() {
    let legacy = MockServer::start().await;
    let body = "hello-from-legacy";
    Mock::given(method("GET"))
        .and(path("/devices/1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&legacy)
        .await;

    let app = router(&legacy_only_config(&legacy.uri()));
    let resp = send(
        &app,
        Request::builder()
            .method("GET")
            .uri("/devices/1")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    let (status, headers, text) = parts(resp).await;
    assert_eq!(status, 200);
    assert_eq!(text, body);
    assert_eq!(
        headers
            .get("content-length")
            .map(|v| v.to_str().unwrap().to_owned()),
        Some(body.len().to_string()),
        "the upstream content-length should be relayed to the client",
    );
}

/// The `/oauth2/auth` split (spec §5.2), end to end: the request's query — not
/// just its path — picks the route, so the one-time-token hops can be pinned to
/// legacy while the ordinary authorize bounces move.
#[tokio::test]
async fn query_conditions_select_the_route() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    for server in [&legacy, &new] {
        Mock::given(method("GET"))
            .and(path("/oauth2/auth"))
            .respond_with(ResponseTemplate::new(200))
            .mount(server)
            .await;
    }

    let config = config_from_yaml(&format!(
        r#"
routes:
  - id: oauth-verifier
    match:
      methods: ["GET"]
      path_prefix: "/oauth2/auth"
      query_present: ["consent_verifier"]
    legacy_upstream: "{legacy}"
    mode: legacy_only
  - id: oauth-authorize
    match: {{ methods: ["GET"], path_prefix: "/oauth2/auth" }}
    new_upstream: "{new}"
    mode: new_only
"#,
        legacy = legacy.uri(),
        new = new.uri()
    ));
    let app = router(&config);

    for uri in [
        "/oauth2/auth?consent_verifier=tok",
        "/oauth2/auth?client_id=app&scope=openid",
    ] {
        let resp = send(
            &app,
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), 200, "{uri}");
    }

    // Same path, same method: only the query separates the two hops.
    let to_legacy = legacy.received_requests().await.unwrap();
    let to_new = new.received_requests().await.unwrap();
    assert_eq!(to_legacy.len(), 1);
    assert_eq!(to_new.len(), 1);
    assert_eq!(to_legacy[0].url.query(), Some("consent_verifier=tok"));
    assert_eq!(to_new[0].url.query(), Some("client_id=app&scope=openid"));
}
