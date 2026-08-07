//! The never-quiesced path (exit 40): a request parked in flight past the
//! deadline must fail the run rather than yield a profile that was still
//! growing.
//!
//! **This is the test that proves identity alone is not the contract.** The
//! profile document below is byte-identical on every poll — the parked request
//! has not been recorded yet, so nothing changes between scrapes — and a
//! quiescence check built on identity alone would happily return it, having
//! classified a route while the observation that could demote it was still in
//! flight. Only `limen_in_flight_requests == 0` catches it.
//!
//! Its own binary, like `verdict_drain_timeout.rs`: it parks a request on the
//! process-global in-flight gauge for seconds, which would poison any parallel
//! test that reads it.

use std::time::Duration;

use limen::config::model::Config;
use limen::draft::{self, ProfileSource, SuggestOptions};
use limen::suggest::{DEFAULT_MAX_COMPARE_PATHS, DEFAULT_MIN_SAMPLES};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::{free_port, spawn_proxy};

#[tokio::test]
async fn a_request_still_in_flight_times_the_quiescence_poll_out() {
    // The upstream answers far past the deadline, so the client request sits in
    // `dispatch` — counted by the in-flight gauge, unrecorded by the profile.
    let slow = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("late")
                .set_delay(Duration::from_secs(20)),
        )
        .mount(&slow)
        .await;

    let data_port = free_port();
    let control_port = free_port();
    let config: Config = serde_yaml::from_str(&format!(
        r#"
server:
  listen_addr: "127.0.0.1:{data_port}"
metrics:
  listen_addr: "127.0.0.1:{control_port}"
  path: "/metrics"
observe: {{}}
routes:
  - id: slow
    match: {{ methods: ["GET"], path_prefix: "/api" }}
    legacy_upstream: "{slow}"
    mode: legacy_only
    timeouts: {{ primary_ms: 30000, shadow_ms: 2000 }}
"#,
        slow = slow.uri(),
    ))
    .unwrap();

    let (_shutdown_tx, _server) = spawn_proxy(config.clone());
    let control_base = format!("http://127.0.0.1:{control_port}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    // Park one request against the slow upstream and wait until the gauge
    // actually reflects it — polling the control plane rather than sleeping,
    // so the test cannot pass by having raced past the request it needs.
    let parked_url = format!("http://127.0.0.1:{data_port}/api/slow");
    let parked = tokio::spawn({
        let client = client.clone();
        async move { client.get(parked_url).send().await }
    });
    let metrics_url = format!("{control_base}/metrics");
    let in_flight = async {
        loop {
            if let Ok(resp) = client.get(&metrics_url).send().await {
                let body = resp.text().await.unwrap_or_default();
                if body
                    .lines()
                    .any(|l| l.trim() == "limen_in_flight_requests 1")
                {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), in_flight)
        .await
        .expect("the parked request should show up on the in-flight gauge");

    let err = draft::run_suggest_routes(
        &config,
        &SuggestOptions {
            source: ProfileSource::ControlPlane {
                base: control_base,
                metrics_path: "/metrics".to_string(),
            },
            min_samples: DEFAULT_MIN_SAMPLES,
            max_compare_paths: DEFAULT_MAX_COMPARE_PATHS,
            drain_deadline: Duration::from_millis(700),
            poll_interval: Duration::from_millis(100),
        },
    )
    .await
    .expect_err("a parked request must not be classified around");
    assert_eq!(err.exit_code(), 40);
    assert!(err.to_string().contains("in flight"), "{err}");

    parked.abort();
}
