//! File-provider flag reload (no restart) and stale-flag fail-safe.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use axum::Router;
use common::{config_from_yaml, parts, send};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn upstream_for(app: &Router, tenant: &str) -> String {
    let resp = send(
        app,
        Request::builder()
            .method("GET")
            .uri("/x")
            .header("x-tenant-id", tenant)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let (_, headers, _) = parts(resp).await;
    headers
        .get("x-upstream")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

async fn upstreams() -> (MockServer, MockServer) {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).insert_header("x-upstream", "legacy"))
        .mount(&legacy)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).insert_header("x-upstream", "new"))
        .mount(&new)
        .await;
    (legacy, new)
}

fn file_config(
    legacy: &str,
    new: &str,
    flags_path: &str,
    stale_ttl_ms: u64,
) -> limen::config::model::Config {
    config_from_yaml(&format!(
        r#"
flags:
  provider: file
  file: {{ path: "{flags_path}", refresh_interval_ms: 1000 }}
  stale_ttl_ms: {stale_ttl_ms}
  fail_safe_mode: legacy_only
routes:
  - id: r
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{new}"
    mode: percentage_split
    rollout:
      percentage_flag: "migration.r.rollout_percentage"
      default_percentage: 0
      assignment_key: {{ header: "x-tenant-id", fallback: request_random }}
"#
    ))
}

#[tokio::test]
async fn file_flag_change_takes_effect_without_restart() {
    let (legacy, new) = upstreams().await;
    let dir = tempfile::tempdir().unwrap();
    let flags_path = dir.path().join("flags.yaml");
    std::fs::write(&flags_path, "migration.r.rollout_percentage: 0\n").unwrap();

    let cfg = file_config(
        &legacy.uri(),
        &new.uri(),
        flags_path.to_str().unwrap(),
        30_000,
    );
    // Build state directly so the test can drive the provider refresh (the poll
    // loop runs only under `serve`).
    let state = limen::http::server::build_state(&cfg, dir.path()).expect("build state");
    let flags = state.flags().clone();
    let app = limen::http::server::data_plane_router(state);

    // Starts at 0% -> legacy for every tenant.
    for t in ["a", "b", "c"] {
        assert_eq!(upstream_for(&app, t).await, "legacy");
    }

    // Update the file to 100% and refresh (what the poll loop does) — no restart.
    std::fs::write(&flags_path, "migration.r.rollout_percentage: 100\n").unwrap();
    flags.refresh().await;

    for t in ["a", "b", "c"] {
        assert_eq!(
            upstream_for(&app, t).await,
            "new",
            "routing follows the flag change"
        );
    }
}

#[tokio::test]
async fn stale_flags_fail_safe_to_legacy() {
    let (legacy, new) = upstreams().await;
    let dir = tempfile::tempdir().unwrap();
    let flags_path = dir.path().join("flags.yaml");
    // 100% rollout, but a 1ms staleness TTL.
    std::fs::write(&flags_path, "migration.r.rollout_percentage: 100\n").unwrap();

    let cfg = file_config(&legacy.uri(), &new.uri(), flags_path.to_str().unwrap(), 1);
    let state = limen::http::server::build_state(&cfg, dir.path()).expect("build state");
    let app = limen::http::server::data_plane_router(state);

    // Let the (1ms) staleness TTL lapse since the initial load.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Despite a 100% rollout, stale flags fail safe to legacy.
    for t in ["a", "b", "c"] {
        assert_eq!(
            upstream_for(&app, t).await,
            "legacy",
            "stale flags must fail safe"
        );
    }
}
