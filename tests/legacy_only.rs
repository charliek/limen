//! `legacy_only` mode: the client is served by legacy; new receives nothing.
//! Method, path, query, and headers are preserved through the proxy.

mod common;

use axum::body::Body;
use axum::http::Request;
use common::{config_from_yaml, parts, router, send};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(legacy: &str, new: &str) -> limen::config::model::Config {
    config_from_yaml(&format!(
        r#"
routes:
  - id: r
    match: {{ methods: ["GET"], path_prefix: "/devices/" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{new}"
    mode: legacy_only
"#
    ))
}

#[tokio::test]
async fn serves_legacy_and_preserves_method_path_query_headers() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;

    // The mock only matches if the method, path, query, and header all arrive
    // intact — so a match is itself the preservation assertion.
    Mock::given(method("GET"))
        .and(path("/devices/1"))
        .and(query_param("verbose", "1"))
        .and(header("x-tenant-id", "t-42"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-upstream", "legacy")
                .set_body_string("legacy-body"),
        )
        .expect(1)
        .mount(&legacy)
        .await;

    let app = router(&config(&legacy.uri(), &new.uri()));
    let resp = send(
        &app,
        Request::builder()
            .method("GET")
            .uri("/devices/1?verbose=1")
            .header("x-tenant-id", "t-42")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    let (status, headers, body) = parts(resp).await;
    assert_eq!(status, 200);
    assert_eq!(headers.get("x-upstream").unwrap(), "legacy"); // response headers preserved
    assert_eq!(body, "legacy-body");

    // legacy got exactly one request; new got none.
    assert_eq!(legacy.received_requests().await.unwrap().len(), 1);
    assert_eq!(new.received_requests().await.unwrap().len(), 0);
}

#[tokio::test]
async fn no_matching_route_returns_404() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    let app = router(&config(&legacy.uri(), &new.uri()));

    let resp = send(
        &app,
        Request::builder()
            .method("GET")
            .uri("/widgets/1")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(resp.status(), 404);
    assert_eq!(legacy.received_requests().await.unwrap().len(), 0);
}
