//! Shared helpers for the integration tests.
//!
//! Tests drive the data-plane router directly via `tower`'s `oneshot` against
//! real `wiremock` upstreams — no production ports are bound, so the tests are
//! fast and isolated. Each test binary uses a subset of these helpers, so the
//! module-level `allow(dead_code)` keeps `-D warnings` happy.
#![allow(dead_code)]

use axum::body::Body;
use axum::http::{HeaderMap, Request, Response, StatusCode};
use axum::Router;
use limen::config::model::Config;
use tower::ServiceExt;

/// Parse a test config from YAML.
pub fn config_from_yaml(yaml: &str) -> Config {
    serde_yaml::from_str(yaml).expect("valid test config")
}

/// Build the data-plane router for a config.
pub fn router(config: &Config) -> Router {
    let state = limen::http::server::build_state(config).expect("build state");
    limen::http::server::data_plane_router(state)
}

/// Send one request through the router (cloning so the router can be reused).
pub async fn send(router: &Router, req: Request<Body>) -> Response<Body> {
    router.clone().oneshot(req).await.expect("router oneshot")
}

/// The status, headers, and body text of a response.
pub async fn parts(resp: Response<Body>) -> (StatusCode, HeaderMap, String) {
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}
