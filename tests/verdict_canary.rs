//! `limen verdict --canary` end-to-end against a real bound server with
//! `debug.sink_canary` enabled: the trigger injects through the live control
//! plane, the record rides the real compare → observer → sink → writer
//! pipeline, and the verdict's drain and relative canary check reconcile it
//! against the engine's counters.
//!
//! Its own test binary (not a phase of `tests/verdict.rs`): the Prometheus
//! recorder is process-global and its counters are monotonic, so a server with
//! the canary enabled needs a process where no other phase has already moved
//! `limen_comparisons_total{route="__limen_canary__"}`.
//!
//! One test function, deliberately: the two runs are ordered phases against
//! one server and one sink — the second's expectations depend on what the
//! first left behind (that re-runnability is the thing being proven).

use std::path::Path;
use std::time::Duration;

use limen::config::model::Config;
use limen::verdict::{self, CheckStatus, VerdictOptions};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::{free_port, spawn_proxy, wait_serving};

/// One compared route over identical upstreams (so every real comparison
/// matches and the only mismatch in the run is the canary), plus the debug
/// block that exposes the endpoint.
fn server_config(data_port: u16, control_port: u16, upstream: &str, sink_dir: &Path) -> Config {
    serde_yaml::from_str(&format!(
        r#"
server:
  listen_addr: "127.0.0.1:{data_port}"
  graceful_shutdown_timeout_ms: 2000
metrics:
  listen_addr: "127.0.0.1:{control_port}"
  path: "/metrics"
diff_sink:
  dir: "{sink}"
debug:
  sink_canary: true
routes:
  - id: hit
    match: {{ methods: ["GET"], path_prefix: "/hit" }}
    legacy_upstream: "{upstream}"
    new_upstream: "{upstream}"
    mode: shadow_legacy_primary
    comparison: {{ enabled: true, sample_rate: 1.0, max_body_bytes: 262144 }}
"#,
        sink = sink_dir.display(),
    ))
    .expect("valid test config")
}

#[tokio::test]
async fn canary_rides_the_real_pipeline_and_is_rerunnable() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("same"))
        .mount(&upstream)
        .await;

    let sink_root = tempfile::tempdir().unwrap();
    let sink_dir = sink_root.path().join("diffs");
    std::fs::create_dir_all(&sink_dir).unwrap();

    let data_port = free_port();
    let control_port = free_port();
    let config = server_config(data_port, control_port, &upstream.uri(), &sink_dir);
    let (shutdown_tx, server) = spawn_proxy(config.clone());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    wait_serving(&client, &format!("http://127.0.0.1:{data_port}/hit")).await;

    // One matching request: the route's default floor of 1 needs a real
    // comparison, so the canary is never the thing satisfying the floors.
    let resp = client
        .get(format!("http://127.0.0.1:{data_port}/hit"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let opts = VerdictOptions {
        sink_dir: sink_dir.clone(),
        control_base: format!("http://127.0.0.1:{control_port}"),
        metrics_path: "/metrics".to_string(),
        canary: true,
        offline: false,
        drain_deadline: Duration::from_secs(10),
        poll_interval: Duration::from_millis(50),
    };

    // Run 1: the trigger injects, the drain covers the injected record, and
    // the canary reconciles — one canary record, zero real mismatches.
    let v1 = verdict::run_verdict(&config, &opts).await.expect("run 1");
    assert_eq!(v1.exit_code, 0, "{v1:?}");
    assert_eq!(v1.canary_records, 1);
    assert_eq!(v1.checks.canary.status, CheckStatus::Pass);
    assert_eq!(v1.checks.drain.status, CheckStatus::Pass);
    assert_eq!(v1.checks.floors.status, CheckStatus::Pass);
    assert_eq!(v1.checks.sink_integrity.status, CheckStatus::Pass);
    assert_eq!(v1.mismatches_total, 0, "the canary is not a real mismatch");

    // Run 2 against the same live proxy and the same (un-reset) sink: the
    // check is relative — sink count == counter count and >= 1 — so a second
    // canary passes at 2 rather than demanding "exactly one".
    let v2 = verdict::run_verdict(&config, &opts).await.expect("run 2");
    assert_eq!(v2.exit_code, 0, "{v2:?}");
    assert_eq!(v2.canary_records, 2);
    assert_eq!(v2.checks.canary.status, CheckStatus::Pass);
    assert_eq!(v2.mismatches_total, 0);

    let _ = shutdown_tx.send(());
    server.await.unwrap().unwrap();
}
