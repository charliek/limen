//! `limen verdict` end-to-end against a real bound server: drive traffic
//! through the data plane, then let the verdict drain the pipeline off the
//! live control plane and reconcile the sink — the full online path a
//! campaign wrapper exercises, minus only the binary spawn (the library
//! return carries the same typed codes the process exits with).
//!
//! One test function, deliberately: the Prometheus recorder and its counters
//! are process-global and monotonic, so distinct scenarios share one server
//! and run as ordered phases (each phase's expectation accounts for the
//! counters the previous phases left behind).

use std::path::Path;
use std::time::Duration;

use limen::config::model::Config;
use limen::verdict::{self, VerdictOptions};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::{free_port, spawn_proxy, wait_serving};

/// The server's config: `hit` compares identical upstreams (always matches),
/// `diff` compares divergent ones (always mismatches on status), `starved`
/// gets no traffic and is floor-exempt here (a later phase re-floors it).
fn server_config(
    data_port: u16,
    control_port: u16,
    same: &str,
    different: &str,
    sink_dir: &Path,
) -> Config {
    serde_yaml::from_str(&format!(
        r#"
server:
  listen_addr: "127.0.0.1:{data_port}"
  graceful_shutdown_timeout_ms: 2000
  request_body_limit_bytes: 1048576
metrics:
  listen_addr: "127.0.0.1:{control_port}"
  path: "/metrics"
diff_sink:
  dir: "{sink}"
routes:
  - id: hit
    match: {{ methods: ["GET"], path_prefix: "/hit" }}
    legacy_upstream: "{same}"
    new_upstream: "{same}"
    mode: shadow_legacy_primary
    comparison: {{ enabled: true, sample_rate: 1.0, max_body_bytes: 262144 }}
  - id: diff
    match: {{ methods: ["GET"], path_prefix: "/diff" }}
    legacy_upstream: "{same}"
    new_upstream: "{different}"
    mode: shadow_legacy_primary
    comparison: {{ enabled: true, sample_rate: 1.0, max_body_bytes: 262144 }}
  - id: starved
    match: {{ methods: ["GET"], path_prefix: "/starved" }}
    legacy_upstream: "{same}"
    new_upstream: "{same}"
    mode: shadow_legacy_primary
    comparison: {{ enabled: true, sample_rate: 1.0, min_comparisons: 0 }}
"#,
        sink = sink_dir.display(),
    ))
    .expect("valid test config")
}

fn options(control_port: u16, sink_dir: &Path) -> VerdictOptions {
    VerdictOptions {
        sink_dir: sink_dir.to_path_buf(),
        control_base: format!("http://127.0.0.1:{control_port}"),
        metrics_path: "/metrics".to_string(),
        canary: false,
        offline: false,
        // Generous ceiling; the drain loop exits on quiescence, not on it.
        drain_deadline: Duration::from_secs(10),
        poll_interval: Duration::from_millis(50),
    }
}

#[tokio::test]
async fn verdict_phases_against_a_live_proxy() {
    let same = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("same"))
        .mount(&same)
        .await;
    let different = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_body_string("different"))
        .mount(&different)
        .await;

    let sink_root = tempfile::tempdir().unwrap();
    let sink_dir = sink_root.path().join("diffs");
    // The runners' contract: the sink dir exists (they reset it with mkdir);
    // verdict reads absence as "wrong path", not "clean".
    std::fs::create_dir_all(&sink_dir).unwrap();

    let data_port = free_port();
    let control_port = free_port();
    let config = server_config(
        data_port,
        control_port,
        &same.uri(),
        &different.uri(),
        &sink_dir,
    );

    let (shutdown_tx, server) = spawn_proxy(config.clone());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    wait_serving(&client, &format!("http://127.0.0.1:{data_port}/hit")).await;

    // Traffic: one more /hit (the readiness probe already sent one) and two
    // /diff mismatches.
    for path in ["/hit", "/diff", "/diff"] {
        let resp = client
            .get(format!("http://127.0.0.1:{data_port}{path}"))
            .send()
            .await
            .unwrap();
        // The client is always served by the legacy primary — mismatches are
        // shadow-side and must never surface here.
        assert_eq!(resp.status(), 200, "{path}");
    }

    let opts = options(control_port, &sink_dir);

    // Phase 1: the verdict drains (no sleep anywhere in this test), floors
    // are met (starved is exempt), the sink agrees with the counters, and
    // the two real mismatches make the verdict exit 10.
    let v1 = verdict::run_verdict(&config, &opts).await.expect("phase 1");
    assert_eq!(v1.exit_code, 10, "{v1:?}");
    assert_eq!(v1.mismatches_total, 2);
    assert_eq!(v1.checks.drain.status, verdict::CheckStatus::Pass);
    assert_eq!(v1.checks.floors.status, verdict::CheckStatus::Pass);
    assert_eq!(v1.checks.sink_integrity.status, verdict::CheckStatus::Pass);
    assert_eq!(v1.sink_mismatches_by_route.get("diff"), Some(&2));

    // Phase 2: same live proxy, but the verdict-side config now floors the
    // starved route at 3 — floors (20) outrank the mismatches (10).
    let mut refloored = config.clone();
    refloored
        .routes
        .iter_mut()
        .find(|r| r.id == "starved")
        .unwrap()
        .comparison
        .min_comparisons = 3;
    let v2 = verdict::run_verdict(&refloored, &opts)
        .await
        .expect("phase 2");
    assert_eq!(v2.exit_code, 20, "{v2:?}");
    assert_eq!(v2.checks.floors.status, verdict::CheckStatus::Fail);
    assert!(v2.checks.floors.detail.contains("starved"));

    // Phase 3: --canary against a proxy without the debug endpoint enabled
    // is a refused injection: typed input-unavailable, never a downgrade.
    let mut canary_opts = opts.clone();
    canary_opts.canary = true;
    let err = verdict::run_verdict(&config, &canary_opts)
        .await
        .expect_err("canary must be refused");
    assert!(err.0.contains("canary"), "{err}");

    // Phase 4: a torn sink line (the proxy killed mid-write) is a sink
    // integrity failure (30), outranking the mismatches.
    let sink_file = std::fs::read_dir(&sink_dir)
        .unwrap()
        .next()
        .expect("phase 1 wrote a sink file")
        .unwrap()
        .path();
    let mut contents = std::fs::read(&sink_file).unwrap();
    contents.extend_from_slice(b"{\"torn\": tru");
    std::fs::write(&sink_file, contents).unwrap();
    let v4 = verdict::run_verdict(&config, &opts).await.expect("phase 4");
    assert_eq!(v4.exit_code, 30, "{v4:?}");
    assert!(v4.checks.sink_integrity.detail.contains("unparseable"));

    // Phase 5: control plane gone — typed input-unavailable again.
    let _ = shutdown_tx.send(());
    server.await.unwrap().unwrap();
    let err = verdict::run_verdict(&config, &opts)
        .await
        .expect_err("dead control plane must be typed");
    assert!(err.0.contains("unreachable"), "{err}");
}
