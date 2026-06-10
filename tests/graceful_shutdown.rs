//! Graceful-shutdown integration test (spec §9, Phase 7 acceptance): on
//! shutdown the proxy stops accepting new connections but lets an in-flight
//! request finish within the drain window, then exits cleanly.

use std::net::{SocketAddr, TcpListener as StdListener};
use std::path::Path;
use std::time::Duration;

use limen::config::model::Config;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Grab a currently-free localhost port by binding to `:0` and releasing it.
fn free_port() -> u16 {
    StdListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Poll until `addr` accepts connections (the server has bound its listener).
async fn wait_connectable(addr: SocketAddr) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("server did not start listening on {addr}");
}

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

    // Drive shutdown from a oneshot rather than an OS signal.
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        limen::http::server::serve_with_shutdown(cfg, Path::new("."), async move {
            let _ = rx.await;
        })
        .await
    });

    let data_addr: SocketAddr = format!("127.0.0.1:{data_port}").parse().unwrap();
    wait_connectable(data_addr).await;

    let url = format!("http://127.0.0.1:{data_port}/devices");

    // Begin a slow request, then trigger shutdown while it is still in flight.
    let inflight = tokio::spawn({
        let url = url.clone();
        async move {
            reqwest::Client::new()
                .get(&url)
                .send()
                .await
                .map(|r| r.status().as_u16())
        }
    });
    tokio::time::sleep(Duration::from_millis(80)).await;
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

    // After shutdown the data plane no longer accepts connections.
    let refused = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_millis(500))
        .send()
        .await;
    assert!(
        refused.is_err(),
        "data plane must stop accepting after shutdown"
    );
}
