//! Graceful-shutdown integration test (spec §9, Phase 7 acceptance): on
//! shutdown the proxy stops accepting new connections but lets an in-flight
//! request finish within the drain window, then exits cleanly.

use std::time::Duration;

use limen::config::model::Config;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::{free_port, spawn_proxy, wait_serving};

#[tokio::test]
async fn drains_in_flight_then_stops_accepting() {
    let legacy = MockServer::start().await;
    // Legacy is slow enough that the request is still in flight when shutdown is
    // triggered, but well under the drain timeout.
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("late-but-ok")
                .set_delay(Duration::from_millis(300)),
        )
        .mount(&legacy)
        .await;

    let data_port = free_port();
    let control_port = free_port();
    let cfg: Config = serde_yaml::from_str(&format!(
        r#"
server:
  listen_addr: "127.0.0.1:{data_port}"
  graceful_shutdown_timeout_ms: 5000
  request_body_limit_bytes: 1048576
metrics:
  listen_addr: "127.0.0.1:{control_port}"
  path: "/metrics"
routes:
  - id: r
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{legacy}"
    mode: legacy_only
"#,
        legacy = legacy.uri()
    ))
    .unwrap();

    // Build both clients before anything time-sensitive. With
    // `rustls-tls-native-roots` the first `Client` construction enumerates the
    // system trust store, which costs ~100ms on macOS; paying that inside the
    // in-flight task would delay its connect past the shutdown signal. The two
    // clients are deliberately separate so the in-flight request opens a fresh
    // connection instead of reusing the probe's pooled one — draining a
    // newly accepted connection is what this test is about.
    let probe = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let inflight_client = reqwest::Client::new();

    // Drive shutdown from a oneshot rather than an OS signal.
    let (tx, server) = spawn_proxy(cfg);

    let url = format!("http://127.0.0.1:{data_port}/devices");
    wait_serving(&probe, &format!("http://127.0.0.1:{data_port}/__ready")).await;

    // Begin a slow request, then trigger shutdown while it is still in flight.
    let inflight = tokio::spawn({
        let url = url.clone();
        async move {
            inflight_client
                .get(&url)
                .send()
                .await
                .map(|r| r.status().as_u16())
        }
    });

    // Shut down on evidence rather than on a timing guess: wait until the
    // upstream has actually logged `/devices`. wiremock records a request under
    // its write lock before matching it, and only awaits the response delay
    // after releasing that lock, so observing the log entry means the request
    // has arrived and the 300ms delay is still running.
    //
    // Residual window, deliberately accepted: the delay starts a moment before
    // this poll can observe the entry, so a host stall longer than the delay
    // could let the response complete before shutdown fires. That would make
    // the drain assertion vacuous rather than failing — a false pass under
    // severe load, not a flake. Removing it entirely needs an upstream that
    // blocks until released, which wiremock's synchronous `Respond` trait
    // cannot express.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let seen = legacy
                .received_requests()
                .await
                .expect("wiremock request recording is on by default")
                .iter()
                .any(|r| r.url.path() == "/devices");
            if seen {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("upstream received /devices before shutdown was triggered");
    tx.send(()).unwrap();

    // The in-flight request drains to completion despite the shutdown.
    let status = tokio::time::timeout(Duration::from_secs(3), inflight)
        .await
        .expect("in-flight request finished in time")
        .expect("in-flight task joined")
        .expect("request succeeded");
    assert_eq!(status, 200, "in-flight request drained successfully");

    // serve() returns cleanly once draining completes.
    let result = tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("serve returned in time")
        .expect("serve task joined");
    assert!(result.is_ok(), "serve exited cleanly: {result:?}");

    // A third, fresh client so no pooled keep-alive socket can satisfy this
    // request and mask a listener that is still accepting.
    let refused = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_millis(500))
        .send()
        .await;
    assert!(
        refused.is_err(),
        "requests must fail after shutdown: the data plane no longer accepts connections"
    );
}
