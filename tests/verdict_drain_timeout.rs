//! The drain-timeout path (exit 40) end-to-end: a shadow held open past the
//! verdict's deadline keeps `limen_shadow_in_flight` at 1, and the verdict
//! must refuse to trust any count rather than report on a half-drained
//! pipeline.
//!
//! Its own binary: it parks a shadow on the process-global in-flight gauge
//! for seconds, which would poison any parallel test sharing the recorder.

use std::time::Duration;

use limen::config::model::Config;
use limen::verdict::{self, CheckStatus, VerdictOptions};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::{free_port, spawn_proxy, wait_serving};

#[tokio::test]
async fn a_hung_shadow_times_the_drain_out() {
    let fast = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&fast)
        .await;
    // The new upstream hangs far past the verdict deadline (but inside the
    // route's shadow timeout, so the shadow stays legitimately in flight).
    let hung = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("late")
                .set_delay(Duration::from_secs(20)),
        )
        .mount(&hung)
        .await;

    let sink_root = tempfile::tempdir().unwrap();
    let sink_dir = sink_root.path().join("diffs");
    std::fs::create_dir_all(&sink_dir).unwrap();

    let data_port = free_port();
    let control_port = free_port();
    let config: Config = serde_yaml::from_str(&format!(
        r#"
server:
  listen_addr: "127.0.0.1:{data_port}"
  graceful_shutdown_timeout_ms: 1000
  request_body_limit_bytes: 1048576
metrics:
  listen_addr: "127.0.0.1:{control_port}"
  path: "/metrics"
diff_sink:
  dir: "{sink}"
routes:
  - id: r
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{fast}"
    new_upstream: "{hung}"
    mode: shadow_legacy_primary
    timeouts: {{ primary_ms: 2000, shadow_ms: 30000 }}
    comparison: {{ enabled: true, sample_rate: 1.0, max_body_bytes: 262144 }}
"#,
        sink = sink_dir.display(),
        fast = fast.uri(),
        hung = hung.uri(),
    ))
    .unwrap();

    let (_shutdown_tx, _server) = spawn_proxy(config.clone());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    wait_serving(&client, &format!("http://127.0.0.1:{data_port}/x")).await;
    // The successful request above dispatched a shadow that is now parked on
    // the hung upstream; the verdict's short deadline elapses under it.
    let opts = VerdictOptions {
        sink_dir: sink_dir.clone(),
        control_base: format!("http://127.0.0.1:{control_port}"),
        metrics_path: "/metrics".to_string(),
        canary: false,
        offline: false,
        drain_deadline: Duration::from_millis(700),
        poll_interval: Duration::from_millis(100),
    };
    let v = verdict::run_verdict(&config, &opts).await.expect("verdict");
    assert_eq!(v.exit_code, 40, "{v:?}");
    assert_eq!(v.verdict, "drain-timeout");
    assert_eq!(v.checks.drain.status, CheckStatus::Fail);
}
