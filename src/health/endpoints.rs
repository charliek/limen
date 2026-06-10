//! The control-plane health handlers and router (spec §10.3).

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;

use crate::health::readiness;

/// `/health/live` — the process is running and able to serve handlers.
async fn live() -> impl IntoResponse {
    (StatusCode::OK, "alive\n")
}

/// `/health/ready` — config is valid and dependencies are usable or in a safe
/// fallback. Returns 200 while serving (ready/degraded) and 503 when unready.
async fn ready() -> impl IntoResponse {
    let state = readiness::evaluate();
    let status = if state.is_serving() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, format!("{}\n", state.label()))
}

/// The control-plane router. `/metrics` is added in Phase 7.
pub fn router() -> Router {
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt; // for `oneshot`

    #[tokio::test]
    async fn live_returns_200() {
        let response = router()
            .oneshot(
                Request::builder()
                    .uri("/health/live")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ready_returns_200_when_serving() {
        let response = router()
            .oneshot(
                Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_control_path_404s() {
        let response = router()
            .oneshot(Request::builder().uri("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
