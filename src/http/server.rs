//! The data-plane and control-plane listeners and shared application state.
//!
//! `serve` binds both planes and runs until a shutdown signal. The router
//! builders ([`build_state`], [`data_plane_router`], [`control_plane_router`])
//! are public so integration tests can drive the proxy via `tower`'s `oneshot`
//! against real upstreams without binding the production ports.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::config::model::Config;
use crate::flags::FlagProvider;
use crate::health::endpoints::{self as health_endpoints, ControlState};
use crate::http::client::UpstreamClient;
use crate::http::proxy;
use crate::observability::{prometheus, MetricsObserver, ShadowObserver};
use crate::resilience::ShadowLimiter;
use crate::routing::RouteTable;

/// Shared, cheaply-cloneable application state for the data plane.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    routes: Arc<RouteTable>,
    client: UpstreamClient,
    shadow_limiter: ShadowLimiter,
    observer: Arc<dyn ShadowObserver>,
    flags: Arc<dyn FlagProvider>,
    request_body_limit: usize,
    shutting_down: AtomicBool,
}

impl AppState {
    /// Construct application state from its parts. The routing table is shared
    /// (behind an `Arc`) with the control plane, which reads per-route breaker
    /// state at scrape time; metrics otherwise flow through the global recorder.
    pub fn new(
        routes: RouteTable,
        client: UpstreamClient,
        shadow_limiter: ShadowLimiter,
        observer: Arc<dyn ShadowObserver>,
        flags: Arc<dyn FlagProvider>,
        request_body_limit: usize,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                routes: Arc::new(routes),
                client,
                shadow_limiter,
                observer,
                flags,
                request_body_limit,
                shutting_down: AtomicBool::new(false),
            }),
        }
    }

    /// The compiled routing table.
    pub fn routes(&self) -> &RouteTable {
        &self.inner.routes
    }

    /// A shared handle to the routing table (for the control plane).
    pub fn routes_arc(&self) -> Arc<RouteTable> {
        self.inner.routes.clone()
    }

    /// The upstream HTTP client.
    pub fn client(&self) -> &UpstreamClient {
        &self.inner.client
    }

    /// The shadow concurrency limiter.
    pub fn shadow_limiter(&self) -> &ShadowLimiter {
        &self.inner.shadow_limiter
    }

    /// The shadow/comparison observer.
    pub fn observer(&self) -> Arc<dyn ShadowObserver> {
        self.inner.observer.clone()
    }

    /// The feature-flag provider.
    pub fn flags(&self) -> &Arc<dyn FlagProvider> {
        &self.inner.flags
    }

    /// The hard cap on buffered request bodies (e.g. for failover replay).
    pub fn request_body_limit(&self) -> usize {
        self.inner.request_body_limit
    }

    /// Whether shutdown has begun (shadows are not started during shutdown).
    pub fn is_shutting_down(&self) -> bool {
        self.inner.shutting_down.load(Ordering::Relaxed)
    }

    /// Mark shutdown as begun.
    pub fn begin_shutdown(&self) {
        self.inner.shutting_down.store(true, Ordering::Relaxed);
    }
}

/// Build the data-plane application state from a (validated) config, using the
/// production [`MetricsObserver`]. `base_dir` resolves relative contract refs.
pub fn build_state(config: &Config, base_dir: &Path) -> anyhow::Result<AppState> {
    let observer: Arc<dyn ShadowObserver> = Arc::new(MetricsObserver::new());
    build_state_with_observer(config, base_dir, observer)
}

/// Build application state with a caller-supplied observer (used by tests to
/// capture comparison outcomes).
pub fn build_state_with_observer(
    config: &Config,
    base_dir: &Path,
    observer: Arc<dyn ShadowObserver>,
) -> anyhow::Result<AppState> {
    let comparisons = crate::routing::resolve_comparisons(config, base_dir)?;
    let routes = RouteTable::build(config, comparisons)?;
    let client = UpstreamClient::build(&config.upstream_tls)?;
    let shadow_limiter = ShadowLimiter::new(config.server.shadow_concurrency_limit);
    let flags = crate::flags::build(&config.flags)?;
    let request_body_limit = config.server.request_body_limit_bytes as usize;
    Ok(AppState::new(
        routes,
        client,
        shadow_limiter,
        observer,
        flags,
        request_body_limit,
    ))
}

/// The data-plane router: a single fallback handler proxies every request.
pub fn data_plane_router(state: AppState) -> Router {
    Router::new().fallback(proxy::handle).with_state(state)
}

/// The control-plane router: health endpoints and the Prometheus `/metrics`
/// endpoint (served at `metrics_path`).
pub fn control_plane_router(control: ControlState, metrics_path: &str) -> Router {
    health_endpoints::router(control, metrics_path)
}

/// Bind both listeners and serve until a SIGINT/SIGTERM, then drain in-flight
/// requests up to `server.graceful_shutdown_timeout_ms` before exiting.
pub async fn serve(config: Config, base_dir: &Path) -> anyhow::Result<()> {
    serve_with_shutdown(config, base_dir, shutdown_signal()).await
}

/// As [`serve`], but driven by a caller-supplied `shutdown` future instead of
/// the OS signal — used by integration tests to trigger a deterministic drain.
pub async fn serve_with_shutdown(
    config: Config,
    base_dir: &Path,
    shutdown: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()> {
    let metrics_handle = prometheus::install();
    let state = build_state(&config, base_dir)?;

    let data_addr: SocketAddr = config.server.listen_addr.parse().map_err(|e| {
        anyhow::anyhow!(
            "invalid server.listen_addr {:?}: {e}",
            config.server.listen_addr
        )
    })?;
    let control_addr: SocketAddr = config.metrics.listen_addr.parse().map_err(|e| {
        anyhow::anyhow!(
            "invalid metrics.listen_addr {:?}: {e}",
            config.metrics.listen_addr
        )
    })?;

    // Fan the shutdown signal out to the servers and the refresh loop.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Start the flag-refresh loop (file/Redis providers). Do one initial,
    // best-effort refresh so values are populated before serving; a failure
    // leaves the provider stale (fail-safe to legacy) until a refresh succeeds.
    // The loop stops promptly when shutdown is signalled.
    let flags = state.flags().clone();
    let refresh_task = match flags.refresh_interval() {
        Some(interval) => {
            flags.refresh().await;
            let mut rx = shutdown_rx.clone();
            Some(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(interval) => flags.refresh().await,
                        _ = rx.changed() => break,
                    }
                }
            }))
        }
        None => None,
    };

    let control_state =
        ControlState::new(state.flags().clone(), state.routes_arc(), metrics_handle);
    let data_app = data_plane_router(state.clone());
    let control_app = control_plane_router(control_state, &config.metrics.path);

    let data_listener = TcpListener::bind(data_addr).await?;
    let control_listener = TcpListener::bind(control_addr).await?;
    info!(
        %data_addr,
        %control_addr,
        metrics_path = %config.metrics.path,
        routes = state.routes().len(),
        "limen listening"
    );

    let data = axum::serve(data_listener, data_app)
        .with_graceful_shutdown(wait_for_shutdown(shutdown_rx.clone()));
    let control = axum::serve(control_listener, control_app)
        .with_graceful_shutdown(wait_for_shutdown(shutdown_rx.clone()));
    let servers = async move { tokio::try_join!(data, control) };
    tokio::pin!(servers);

    // Serve until shutdown is requested (or a listener fails on its own).
    tokio::select! {
        res = &mut servers => {
            res?;
            info!("limen stopped");
            return Ok(());
        }
        _ = shutdown => {}
    }

    // Drain: stop starting new shadows, end the refresh loop, and trigger the
    // servers' graceful shutdown — bounded by the configured drain timeout.
    state.begin_shutdown();
    let _ = shutdown_tx.send(true);
    let drain = Duration::from_millis(config.server.graceful_shutdown_timeout_ms);
    match tokio::time::timeout(drain, &mut servers).await {
        Ok(res) => {
            res?;
            info!("in-flight requests drained; limen stopped");
        }
        // On timeout we stop waiting and return. Any still-open connection tasks
        // are abandoned and torn down when the process exits (the intended
        // "forcing exit"). A library embedder that keeps running after this call
        // should treat the bound as best-effort for that reason.
        Err(_) => warn!(
            timeout_ms = config.server.graceful_shutdown_timeout_ms,
            "graceful shutdown timeout exceeded; forcing exit"
        ),
    }
    if let Some(task) = refresh_task {
        task.abort();
    }
    Ok(())
}

/// Resolve once the shutdown flag flips to `true` (used as each server's
/// graceful-shutdown trigger). Returns immediately if it is already set.
async fn wait_for_shutdown(mut rx: watch::Receiver<bool>) {
    if *rx.borrow_and_update() {
        return;
    }
    let _ = rx.changed().await;
}

/// Resolve when the process receives SIGINT (Ctrl-C) or SIGTERM. The caller then
/// triggers the bounded drain (stop accepting, finish in-flight up to the
/// timeout).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("shutdown signal received; draining in-flight requests");
}
