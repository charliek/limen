//! Shared helpers for the integration tests.
//!
//! Tests drive the data-plane router directly via `tower`'s `oneshot` against
//! real `wiremock` upstreams — no production ports are bound, so the tests are
//! fast and isolated. The exception is [`raw_upstream`], for the properties no
//! mock template can produce. Each test binary uses a subset of these helpers,
//! so the module-level `allow(dead_code)` keeps `-D warnings` happy.
#![allow(dead_code)]

use std::future::Future;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Request, Response, StatusCode};
use axum::Router;
use limen::config::model::Config;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tower::ServiceExt;

/// Parse a test config from YAML.
pub fn config_from_yaml(yaml: &str) -> Config {
    serde_yaml::from_str(yaml).expect("valid test config")
}

/// Grab a currently-free localhost port by binding to `:0` and releasing it.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Spawn a real bound proxy (data plane + control plane) for `config`. The
/// returned sender triggers graceful shutdown — as does dropping it — and the
/// handle carries `serve_with_shutdown`'s result. For the tests that bind
/// ports rather than driving the router directly via `oneshot`.
pub fn spawn_proxy(
    config: Config,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        limen::http::server::serve_with_shutdown(config, std::path::Path::new("."), async move {
            let _ = rx.await;
        })
        .await
    });
    (tx, handle)
}

/// Poll until the proxy actually serves a request. A bare TCP connect only
/// proves the kernel completed a handshake into the accept backlog, which can
/// succeed before the server is accepting — only a successful response proves
/// the data plane is live.
/// One outer deadline rather than a bounded attempt count: each attempt can
/// itself block for the client's request timeout, so "100 attempts" would be an
/// unpredictable ceiling rather than a bound.
pub async fn wait_serving(client: &reqwest::Client, url: &str) {
    let probing = async {
        loop {
            if let Ok(resp) = client.get(url).send().await {
                if resp.status().is_success() {
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), probing)
        .await
        .unwrap_or_else(|_| panic!("proxy did not start serving requests at {url}"));
}

/// A single `shadow_legacy_primary` route over the two upstreams, comparing
/// every request — the shape every shadow-path test starts from.
pub fn shadow_config(legacy: &str, new: &str, shadow_ms: u64) -> Config {
    config_from_yaml(&format!(
        r#"
routes:
  - id: r
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{new}"
    mode: shadow_legacy_primary
    timeouts: {{ primary_ms: 2000, shadow_ms: {shadow_ms} }}
    comparison: {{ enabled: true, sample_rate: 1.0, max_body_bytes: 262144 }}
"#
    ))
}

/// Build the data-plane router for a config (no contract refs in tests, so the
/// base dir is irrelevant).
pub fn router(config: &Config) -> Router {
    let state =
        limen::http::server::build_state(config, std::path::Path::new(".")).expect("build state");
    limen::http::server::data_plane_router(state)
}

/// Build a data-plane router with a caller-supplied shadow observer (for tests
/// that assert on comparison outcomes).
pub fn router_with_observer(
    config: &Config,
    observer: std::sync::Arc<dyn limen::observability::ShadowObserver>,
) -> Router {
    let state =
        limen::http::server::build_state_with_observer(config, std::path::Path::new("."), observer)
            .expect("build state");
    limen::http::server::data_plane_router(state)
}

/// Send one request through the router (cloning so the router can be reused).
pub async fn send(router: &Router, req: Request<Body>) -> Response<Body> {
    router.clone().oneshot(req).await.expect("router oneshot")
}

/// The value of one exposition line, keyed by everything left of the value
/// (`limen_shadow_in_flight`, `limen_diff_sink_dropped_total{reason="io_error"}`).
/// `None` means the series is absent — which a verdict must never conflate with
/// zero, so tests assert on the distinction.
pub fn metric_value(rendered: &str, series: &str) -> Option<f64> {
    rendered.lines().find_map(|line| {
        line.strip_prefix(series)?
            .strip_prefix(' ')?
            .trim()
            .parse()
            .ok()
    })
}

/// Poll `cond` every 10ms for up to ~5s, so a fire-and-forget shadow task (or
/// the sink's writer thread) can make progress. Panics with `what` on timeout.
pub async fn wait_until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// A gate the test opens when it wants an upstream to continue. A `watch`
/// rather than a `oneshot` so one upstream can gate several connections.
#[derive(Clone)]
pub struct Gate(tokio::sync::watch::Sender<bool>);

impl Gate {
    pub fn new() -> Self {
        Self(tokio::sync::watch::channel(false).0)
    }
    pub fn open(&self) {
        self.0.send_replace(true);
    }
    pub async fn wait(&self) {
        let mut rx = self.0.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// A raw-TCP upstream: accepts connections for the lifetime of the test and
/// hands each one, plus its request head, to `serve`. Returns the origin URL.
///
/// Raw sockets rather than `wiremock` because the properties these tests pin
/// need response shapes no mock template can produce — a body held
/// half-written, a stream that never ends, a socket closed mid-body.
pub async fn raw_upstream<F, Fut>(serve: F) -> String
where
    F: Fn(TcpStream, String) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve = Arc::new(serve);
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let serve = serve.clone();
            tokio::spawn(async move {
                if let Some(head) = read_head(&mut sock).await {
                    serve(sock, head).await;
                }
            });
        }
    });
    format!("http://{addr}")
}

/// Read a request head off a raw socket. The test requests carry no body, so
/// the head is the whole request; `None` means the peer hung up first.
async fn read_head(sock: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        head.extend_from_slice(&buf[..n]);
    }
    Some(String::from_utf8_lossy(&head).into_owned())
}

/// Write raw bytes to a socket and flush, so the peer sees them before the
/// handler goes on to wait on whatever comes next.
pub async fn write(sock: &mut TcpStream, data: &str) {
    sock.write_all(data.as_bytes()).await.unwrap();
    sock.flush().await.unwrap();
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
