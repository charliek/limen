//! `percentage_split` mode: deterministic per-tenant assignment from a flag,
//! served by exactly one upstream (no doubled side effect).

mod common;

use axum::body::Body;
use axum::http::Request;
use axum::Router;
use common::{config_from_yaml, parts, router, send};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(legacy: &str, new: &str, percentage: u32) -> limen::config::model::Config {
    config_from_yaml(&format!(
        r#"
flags:
  provider: static
  static:
    values:
      "migration.r.rollout_percentage": {percentage}
  stale_ttl_ms: 30000
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

async fn upstream_for(app: &Router, tenant: Option<&str>) -> String {
    let mut builder = Request::builder().method("GET").uri("/x");
    if let Some(t) = tenant {
        builder = builder.header("x-tenant-id", t);
    }
    let resp = send(app, builder.body(Body::empty()).unwrap()).await;
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

#[tokio::test]
async fn zero_percent_is_all_legacy_hundred_is_all_new() {
    let (legacy, new) = upstreams().await;

    let all_legacy = router(&config(&legacy.uri(), &new.uri(), 0));
    for t in ["a", "b", "c"] {
        assert_eq!(upstream_for(&all_legacy, Some(t)).await, "legacy");
    }

    let all_new = router(&config(&legacy.uri(), &new.uri(), 100));
    for t in ["a", "b", "c"] {
        assert_eq!(upstream_for(&all_new, Some(t)).await, "new");
    }
}

#[tokio::test]
async fn same_tenant_is_stable_across_requests() {
    let (legacy, new) = upstreams().await;
    let app = router(&config(&legacy.uri(), &new.uri(), 50));
    let first = upstream_for(&app, Some("tenant-stable")).await;
    for _ in 0..10 {
        assert_eq!(upstream_for(&app, Some("tenant-stable")).await, first);
    }
}

#[tokio::test]
async fn many_tenants_distribute_near_the_percentage() {
    let (legacy, new) = upstreams().await;
    let app = router(&config(&legacy.uri(), &new.uri(), 50));
    let mut new_count = 0;
    let total = 200;
    for i in 0..total {
        if upstream_for(&app, Some(&format!("tenant-{i}"))).await == "new" {
            new_count += 1;
        }
    }
    // ~50% of 200, with generous tolerance for hash variance.
    assert!(
        (70..130).contains(&new_count),
        "expected ~100 of {total} to new, got {new_count}"
    );
}
