//! The data-plane and control-plane listeners and shared application state.
//!
//! `serve` binds both planes and runs until a shutdown signal. The router
//! builders ([`build_state`], [`data_plane_router`], [`control_plane_router`])
//! are public so integration tests can drive the proxy via `tower`'s `oneshot`
//! against real upstreams without binding the production ports.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use tokio::net::TcpListener;
use tracing::info;

use crate::config::model::Config;
use crate::health::endpoints as health_endpoints;
use crate::http::client::UpstreamClient;
use crate::http::proxy;
use crate::routing::RouteTable;

/// Shared, cheaply-cloneable application state for the data plane.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    routes: RouteTable,
    client: UpstreamClient,
}

impl AppState {
    /// Construct from a compiled route table and an upstream client.
    pub fn new(routes: RouteTable, client: UpstreamClient) -> Self {
        Self {
            inner: Arc::new(Inner { routes, client }),
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
}

/// Build the data-plane application state from a (validated) config.
pub fn build_state(config: &Config) -> anyhow::Result<AppState> {
    let routes = RouteTable::build(config)?;
    let client = UpstreamClient::build(&config.upstream_tls)?;
    Ok(AppState::new(routes, client))
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
pub async fn serve(config: Config) -> anyhow::Result<()> {
    let state = build_state(&config)?;

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

    let data = axum::serve(data_listener, data_app).with_graceful_shutdown(shutdown_signal());
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
