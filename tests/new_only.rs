//! `new_only` mode: the client is served by new; legacy receives nothing.
//! A POST body is preserved through the proxy.

mod common;

use axum::body::Body;
use axum::http::Request;
use common::{config_from_yaml, parts, router, send};
use wiremock::matchers::{body_string, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn serves_new_and_preserves_post_body() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/devices"))
        .and(body_string("{\"name\":\"a\"}"))
        .respond_with(
            ResponseTemplate::new(201)
                .insert_header("x-upstream", "new")
                .set_body_string("created"),
        )
        .expect(1)
        .mount(&new)
        .await;

    let app = router(&config_from_yaml(&format!(
        r#"
routes:
  - id: r
    match: {{ methods: ["POST"], path_prefix: "/devices" }}
    legacy_upstream: "{}"
    new_upstream: "{}"
    mode: new_only
"#,
        legacy.uri(),
        new.uri()
    )));

    let resp = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/devices")
            .header("content-type", "application/json")
            .body(Body::from("{\"name\":\"a\"}"))
            .unwrap(),
    )
    .await;

    let (status, headers, body) = parts(resp).await;
    assert_eq!(status, 201);
    assert_eq!(headers.get("x-upstream").unwrap(), "new");
    assert_eq!(body, "created");

    assert_eq!(new.received_requests().await.unwrap().len(), 1);
    assert_eq!(legacy.received_requests().await.unwrap().len(), 0);
}
