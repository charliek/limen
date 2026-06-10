//! The control-plane handlers and router: health checks and `/metrics`
//! (spec §10.1, §10.3).

use std::sync::Arc;

use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use metrics_exporter_prometheus::PrometheusHandle;

use crate::flags::FlagProvider;
use crate::health::readiness;
use crate::observability::prometheus;
use crate::routing::RouteTable;

/// Shared, cheaply-cloneable state for the control plane.
#[derive(Clone)]
pub struct ControlState {
    flags: Arc<dyn FlagProvider>,
    routes: Arc<RouteTable>,
    metrics: PrometheusHandle,
}

impl ControlState {
    /// Assemble control-plane state from the flag provider, routing table, and
    /// the Prometheus render handle.
    pub fn new(
        flags: Arc<dyn FlagProvider>,
        routes: Arc<RouteTable>,
        metrics: PrometheusHandle,
    ) -> Self {
        Self {
            flags,
            routes,
            metrics,
        }
    }
}

/// `/health/live` — the process is running and able to serve handlers.
async fn live() -> impl IntoResponse {
    (StatusCode::OK, "alive\n")
}

/// `/health/ready` — config is valid and dependencies are usable or in a safe
/// fallback. Returns 200 while serving (ready/degraded) and 503 when unready.
async fn ready(State(control): State<ControlState>) -> impl IntoResponse {
    let health = control.flags.health();
    let state = readiness::evaluate(Some(&health));
    let status = if state.is_serving() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, format!("{}\n", state.label()))
}

/// `/metrics` — Prometheus exposition. Point-in-time gauges (breaker state, flag
/// health) are refreshed at scrape time so they reflect the moment of the
/// scrape rather than a stale periodic sample.
async fn metrics(State(control): State<ControlState>) -> impl IntoResponse {
    for route in control.routes.iter() {
        if let Some(breaker) = &route.breaker {
            prometheus::set_breaker_state(&route.id, breaker.state());
        }
    }
    let health = control.flags.health();
    prometheus::set_flag_health(
        health.stale,
        health.last_success_age_ms.map(|ms| ms as f64 / 1000.0),
        health.consecutive_failures,
    );
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        control.metrics.render(),
    )
}

/// The control-plane router: health checks plus the metrics endpoint at
/// `metrics_path`.
pub fn router(control: ControlState, metrics_path: &str) -> Router {
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .route(metrics_path, get(metrics))
        .with_state(control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flags::{FlagProvider, FlagProviderHealth, FlagValue};
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use metrics_exporter_prometheus::PrometheusBuilder;
    use std::time::Duration;
    use tower::ServiceExt; // for `oneshot`

    struct StaleFlags;
    #[async_trait]
    impl FlagProvider for StaleFlags {
        async fn get(&self, _key: &str) -> Option<FlagValue> {
            None
        }
        fn health(&self) -> FlagProviderHealth {
            FlagProviderHealth {
                stale: true,
                last_success_age_ms: Some(60_000),
                consecutive_failures: 3,
            }
        }
        async fn refresh(&self) {}
        fn refresh_interval(&self) -> Option<Duration> {
            None
        }
    }

    fn control() -> ControlState {
        ControlState::new(
            Arc::new(StaleFlags),
            Arc::new(RouteTable::default()),
            PrometheusBuilder::new().build_recorder().handle(),
        )
    }

    async fn get_path(path: &str) -> axum::http::Response<Body> {
        router(control(), "/metrics")
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn live_returns_200() {
        assert_eq!(get_path("/health/live").await.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ready_degraded_still_returns_200() {
        // A stale flag provider degrades but still serves (legacy), so ready=200.
        assert_eq!(get_path("/health/ready").await.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn metrics_endpoint_renders() {
        let resp = get_path("/metrics").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            "text/plain; version=0.0.4; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn unknown_control_path_404s() {
        assert_eq!(get_path("/nope").await.status(), StatusCode::NOT_FOUND);
    }
}
