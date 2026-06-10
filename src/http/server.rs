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

use axum::Router;
use tokio::net::TcpListener;
use tracing::info;

use crate::config::model::Config;
use crate::flags::FlagProvider;
use crate::health::endpoints as health_endpoints;
use crate::http::client::UpstreamClient;
use crate::http::proxy;
use crate::observability::{MetricsObserver, ShadowObserver, Stats};
use crate::resilience::ShadowLimiter;
use crate::routing::RouteTable;

/// Shared, cheaply-cloneable application state for the data plane.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    routes: RouteTable,
    client: UpstreamClient,
    shadow_limiter: ShadowLimiter,
    observer: Arc<dyn ShadowObserver>,
    flags: Arc<dyn FlagProvider>,
    shutting_down: AtomicBool,
}

impl AppState {
    /// Construct application state from its parts. The observer owns the stats
    /// counters; the control plane reads them via [`ShadowObserver::snapshot`].
    pub fn new(
        routes: RouteTable,
        client: UpstreamClient,
        shadow_limiter: ShadowLimiter,
        observer: Arc<dyn ShadowObserver>,
        flags: Arc<dyn FlagProvider>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                routes,
                client,
                shadow_limiter,
                observer,
                flags,
                shutting_down: AtomicBool::new(false),
            }),
        }
    }

    /// The compiled routing table.
    pub fn routes(&self) -> &RouteTable {
        &self.inner.routes
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
    let observer: Arc<dyn ShadowObserver> =
        Arc::new(MetricsObserver::new(Arc::new(Stats::default())));
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
    Ok(AppState::new(
        routes,
        client,
        shadow_limiter,
        observer,
        flags,
    ))
}

/// The data-plane router: a single fallback handler proxies every request.
pub fn data_plane_router(state: AppState) -> Router {
    Router::new().fallback(proxy::handle).with_state(state)
}

/// The control-plane router: health endpoints (and, from Phase 7, `/metrics`).
pub fn control_plane_router() -> Router {
    health_endpoints::router()
}

/// Bind both listeners and serve until a shutdown signal arrives.
pub async fn serve(config: Config, base_dir: &Path) -> anyhow::Result<()> {
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

    // Start the flag-refresh loop (file/Redis providers). Do one initial,
    // best-effort refresh so values are populated before serving; a failure
    // leaves the provider stale (fail-safe to legacy) until a refresh succeeds.
    let flags = state.flags().clone();
    if let Some(interval) = flags.refresh_interval() {
        flags.refresh().await;
        // Phase 7: tie this background task into graceful shutdown (hold its
        // JoinHandle / abort on drain). It is detached for now.
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                flags.refresh().await;
            }
        });
    }

    let data_app = data_plane_router(state.clone());
    let control_app = control_plane_router();

    let data_listener = TcpListener::bind(data_addr).await?;
    let control_listener = TcpListener::bind(control_addr).await?;
    info!(
        %data_addr,
        %control_addr,
        routes = state.routes().len(),
        "limen listening"
    );

    // On the signal, flag shutdown so the data plane stops starting new
    // shadows, then let axum drain in-flight requests.
    let data_shutdown = {
        let state = state.clone();
        async move {
            shutdown_signal().await;
            state.begin_shutdown();
        }
    };
    let data = axum::serve(data_listener, data_app).with_graceful_shutdown(data_shutdown);
    let control =
        axum::serve(control_listener, control_app).with_graceful_shutdown(shutdown_signal());

    // Both serve futures share `io::Error`; `?` converts to `anyhow::Error`.
    tokio::try_join!(data, control)?;

    info!("limen stopped");
    Ok(())
}

/// Resolve when the process receives SIGINT (Ctrl-C) or SIGTERM. Full in-flight
/// draining is hardened in Phase 7; `axum::serve`'s graceful shutdown already
/// stops accepting and lets in-flight requests finish.
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
