//! The control-plane handlers and router: health checks and `/metrics`
//! (spec §10.1, §10.3).

use std::sync::Arc;

use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use metrics_exporter_prometheus::PrometheusHandle;
use serde_json::json;
use tracing::error;

use crate::compare::diff::DiffLimits;
use crate::compare::{self, Captured};
use crate::config::model::FailSafeMode;
use crate::contract::model::ComparisonRules;
use crate::flags::FlagProvider;
use crate::health::readiness;
use crate::observability::observe::OBSERVE_PROFILE_PATH;
use crate::observability::request_id;
use crate::observability::{prometheus, ObserveRecorder, ShadowMeta, ShadowObserver};
use crate::routing::{rollout, RouteTable};
use crate::verdict::CANARY_ROUTE_ID;

/// `/health/live` — see [`live`].
pub const HEALTH_LIVE_PATH: &str = "/health/live";
/// `/health/ready` — see [`ready`].
pub const HEALTH_READY_PATH: &str = "/health/ready";
/// `POST /debug/canary` — see [`debug_canary`].
pub const DEBUG_CANARY_PATH: &str = "/debug/canary";

/// Every path [`router`] always registers, regardless of config — i.e.
/// everything but the operator-supplied `metrics_path`. Lives here (rather
/// than in `config::validate`, the only other module that cares) because
/// `router` is the thing making the promise; validation just needs to read
/// it. [`router`] builds its fixed routes from these constants, and
/// `config::validate` checks `metrics.path` against them, so the two cannot
/// drift into the panic this was written to prevent — axum panics at router
/// *build* time on a duplicate route, so a collision must be caught at
/// config-validation time instead (invariant 7).
///
/// `/observe/profile` is deliberately not here: it is only registered when
/// the observe block is present, so it stays a conditional check in
/// `config::validate::validate_observe` against
/// [`crate::observability::observe::OBSERVE_PROFILE_PATH`] directly.
pub const CONTROL_PLANE_RESERVED_PATHS: &[&str] =
    &[HEALTH_LIVE_PATH, HEALTH_READY_PATH, DEBUG_CANARY_PATH];

/// Shared, cheaply-cloneable state for the control plane.
#[derive(Clone)]
pub struct ControlState {
    flags: Arc<dyn FlagProvider>,
    routes: Arc<RouteTable>,
    metrics: PrometheusHandle,
    /// The fail-safe mode the data plane routes by. Held so the scrape-time
    /// rollout gauge resolves through exactly the inputs `decide_primary` uses
    /// — a control plane guessing this could report a rollout the router is
    /// not performing.
    fail_safe_mode: FailSafeMode,
    /// The data plane's shadow observer, present **only** when
    /// `debug.sink_canary` is enabled. `Some` is the whole enablement signal:
    /// with the block off the control plane cannot reach the pipeline at all,
    /// rather than holding a capability it promises not to use.
    canary_observer: Option<Arc<dyn ShadowObserver>>,
    /// The observe-mode recorder, present **only** when the `observe:` block
    /// is configured. Same `Some`-is-the-switch idiom, one step further: this
    /// one also decides whether [`router`] registers the profile route, which
    /// is what keeps `validate_observe`'s conditional `metrics.path` collision
    /// check honest — with observation off, nothing claims that path.
    observe: Option<Arc<ObserveRecorder>>,
}

impl ControlState {
    /// Assemble control-plane state from the flag provider, routing table, the
    /// Prometheus render handle, and the data plane's fail-safe mode. The debug
    /// canary is off unless [`ControlState::with_sink_canary`] adds it.
    pub fn new(
        flags: Arc<dyn FlagProvider>,
        routes: Arc<RouteTable>,
        metrics: PrometheusHandle,
        fail_safe_mode: FailSafeMode,
    ) -> Self {
        Self {
            flags,
            routes,
            metrics,
            fail_safe_mode,
            canary_observer: None,
            observe: None,
        }
    }

    /// Enable `POST /debug/canary`, injecting through `observer` — which must
    /// be the *same* observer the shadow path uses (the production `Fanout`),
    /// or the canary would prove something about a pipeline nobody runs.
    pub fn with_sink_canary(mut self, observer: Arc<dyn ShadowObserver>) -> Self {
        self.canary_observer = Some(observer);
        self
    }

    /// Serve `GET /observe/profile` from `recorder` — which must be the *same*
    /// recorder the proxy writes to, or the profile would describe traffic
    /// nobody sent.
    pub fn with_observe(mut self, recorder: Arc<ObserveRecorder>) -> Self {
        self.observe = Some(recorder);
        self
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

/// `/metrics` — Prometheus exposition. Point-in-time gauges (breaker state, the
/// resolved rollout target, flag health) are refreshed at scrape time so they
/// reflect the moment of the scrape rather than a stale periodic sample.
async fn metrics(State(control): State<ControlState>) -> impl IntoResponse {
    for route in control.routes.iter() {
        if let Some(breaker) = &route.breaker {
            prometheus::set_breaker_state(&route.id, breaker.state());
        }
        // Resolved through the *router's* function, not a copy of its logic:
        // the gauge and the routing decision are two readings of one
        // resolution, so they cannot drift apart.
        if let Some(rollout) = route.rollout_target() {
            let resolved = rollout::resolve_percentage(
                rollout,
                control.flags.as_ref(),
                control.fail_safe_mode,
            )
            .await;
            prometheus::set_rollout_target_percentage(&route.id, resolved);
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

/// `POST /debug/canary` — inject one synthetic mismatch through the real
/// comparison and observer pipeline, so `limen verdict --canary` can prove the
/// record → flush → report path bites *right now* rather than assuming it.
///
/// 404 unless `debug.sink_canary` is set: an operator who never asked for the
/// endpoint sees no trace of it.
///
/// **Synchronous through `observer.comparison` on purpose.** Verdict triggers
/// the canary and then immediately starts draining; if the injection were
/// spawned, the drain could scrape a balanced, idle pipeline before the record
/// was ever offered, call it drained, and then fail integrity on a canary that
/// landed afterwards. Offering the record before the POST returns makes the
/// trigger's completion a real happens-before edge for the drain.
async fn debug_canary(State(control): State<ControlState>, headers: HeaderMap) -> Response {
    let Some(observer) = control.canary_observer.clone() else {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    };

    // A pair that differs on both dimensions the default rules compare
    // (status and body), so no contract or route config is involved in making
    // it a mismatch — the canary must not depend on the config under test.
    let legacy = Captured {
        status: 200,
        headers: HeaderMap::new(),
        body: Bytes::from_static(br#"{"canary":"legacy"}"#),
        request_url: None,
    };
    let new = Captured {
        status: 500,
        headers: HeaderMap::new(),
        body: Bytes::from_static(br#"{"canary":"new"}"#),
        request_url: None,
    };
    let result = compare::compare(
        &ComparisonRules::default(),
        &DiffLimits::default(),
        &legacy,
        &new,
    );
    // Checked at runtime, not `debug_assert`: campaigns run release builds,
    // which is exactly where a comparison engine that stopped flagging this
    // would have to be caught. Refusing here makes verdict exit 50 (input
    // refused) instead of injecting a "mismatch" that is not one.
    if result.is_match() {
        error!("debug canary: the comparison engine did not flag a deliberately divergent pair");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "canary pair did not compare as a mismatch\n",
        )
            .into_response();
    }

    let meta = ShadowMeta {
        route_id: CANARY_ROUTE_ID.to_string(),
        // Reuses a caller-supplied `x-request-id` when there is one (so a
        // runner can correlate), else a fresh id — every injection is
        // distinguishable in the sink.
        request_id: format!("canary-{}", request_id::resolve(&headers)),
        method: Method::POST,
        path: "/debug/canary".to_string(),
    };
    observer.comparison(&meta, &result);

    (
        StatusCode::OK,
        Json(json!({ "injected": true, "route_id": CANARY_ROUTE_ID })),
    )
        .into_response()
}

/// `GET /observe/profile` — the whole observe-mode profile as one JSON
/// document: every configured route, zero-filled until observed, canonically
/// ordered and carrying no wall-clock field so two scrapes of an idle proxy are
/// byte-identical (`limen suggest-routes` polls for exactly that).
///
/// Registered only while observe mode is on, so with the block absent the path
/// does not exist at all — which is also why `metrics.path` may collide with it
/// only under the same condition.
async fn observe_profile(State(control): State<ControlState>) -> Response {
    // Unreachable: `router` registers this path only when the recorder is
    // present. The arm exists because `ControlState` cannot express "present
    // on this route", not as a second enablement check.
    let Some(recorder) = control.observe.as_ref() else {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    };
    Json(recorder.profile()).into_response()
}

/// The control-plane router: health checks, the metrics endpoint at
/// `metrics_path`, the debug canary (which 404s unless enabled), and — only
/// while observe mode is on — the profile endpoint.
pub fn router(control: ControlState, metrics_path: &str) -> Router {
    let observing = control.observe.is_some();
    let mut router = Router::new()
        .route(HEALTH_LIVE_PATH, get(live))
        .route(HEALTH_READY_PATH, get(ready))
        .route(metrics_path, get(metrics))
        .route(DEBUG_CANARY_PATH, post(debug_canary));
    if observing {
        router = router.route(OBSERVE_PROFILE_PATH, get(observe_profile));
    }
    router.with_state(control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::result::ComparisonResult;
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
            FailSafeMode::LegacyOnly,
        )
    }

    async fn get_path(path: &str) -> axum::http::Response<Body> {
        router(control(), "/metrics")
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    /// Records every comparison it is handed, synchronously.
    #[derive(Default)]
    struct Capture(std::sync::Mutex<Vec<(ShadowMeta, ComparisonResult)>>);

    impl ShadowObserver for Capture {
        fn shadow_dispatched(&self, _: &ShadowMeta) {}
        fn comparison(&self, meta: &ShadowMeta, result: &ComparisonResult) {
            self.0.lock().unwrap().push((meta.clone(), result.clone()));
        }
        fn shadow_skipped(&self, _: &ShadowMeta, _: crate::observability::SkipReason) {}
        fn shadow_failed(&self, _: &ShadowMeta, _: crate::observability::ShadowFailure) {}
        fn comparison_skipped(&self, _: &ShadowMeta, _: crate::observability::SkipReason) {}
    }

    async fn post_canary(control: ControlState) -> axum::http::Response<Body> {
        router(control, "/metrics")
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/debug/canary")
                    .body(Body::empty())
                    .unwrap(),
            )
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

    #[tokio::test]
    async fn observe_profile_is_absent_unless_enabled() {
        // Not "registered and 404ing from inside": with observe off the route
        // is never registered, which is what lets `metrics.path` legitimately
        // take `/observe/profile` in that configuration.
        assert_eq!(
            get_path(OBSERVE_PROFILE_PATH).await.status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn enabled_observe_profile_serves_every_configured_route_zero_filled() {
        use crate::config::model::ObserveConfig;
        use crate::observability::ObserveRecorder;
        use crate::routing::PathMatcher;

        let alpha = PathMatcher::Prefix("/alpha".to_string());
        let beta = PathMatcher::Prefix("/beta".to_string());
        let recorder = Arc::new(ObserveRecorder::new(
            ObserveConfig::default(),
            [("alpha", &alpha), ("beta", &beta)],
        ));
        let resp = router(control().with_observe(recorder), "/metrics")
            .oneshot(
                Request::builder()
                    .uri(OBSERVE_PROFILE_PATH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["routes"]["alpha"]["observations"], 0);
        assert_eq!(json["routes"]["beta"]["observations"], 0);
        assert!(
            json["routes"]["gamma"].is_null(),
            "a route nobody configured must be absent, not zero"
        );
    }

    #[tokio::test]
    async fn canary_404s_unless_enabled() {
        assert_eq!(post_canary(control()).await.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn enabled_canary_injects_one_reserved_mismatch() {
        let capture = Arc::new(Capture::default());
        let resp = post_canary(control().with_sink_canary(capture.clone())).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["injected"], true);
        assert_eq!(json["route_id"], CANARY_ROUTE_ID);

        // The observer effect is complete by the time the response is in hand:
        // no polling, no `wait_until` — anything else would mean verdict could
        // drain past an injection still in flight (the H4 synchronicity pin).
        let seen = capture.0.lock().unwrap();
        assert_eq!(seen.len(), 1, "exactly one injection per trigger");
        let (meta, result) = &seen[0];
        assert_eq!(meta.route_id, CANARY_ROUTE_ID);
        assert_eq!(meta.method, Method::POST);
        assert!(meta.request_id.starts_with("canary-"));
        assert!(!result.is_match(), "the canary must be a mismatch");
        assert!(
            !result.status_match,
            "200 vs 500 is the injected difference"
        );
    }

    #[tokio::test]
    async fn each_trigger_injects_exactly_once() {
        // Re-runnable by design: verdict's canary check is relative (sink count
        // == counter count >= 1), so N triggers must produce exactly N records.
        let capture = Arc::new(Capture::default());
        let control = control().with_sink_canary(capture.clone());
        for _ in 0..2 {
            assert_eq!(post_canary(control.clone()).await.status(), StatusCode::OK);
        }
        let seen = capture.0.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_ne!(
            seen[0].0.request_id, seen[1].0.request_id,
            "each injection is individually identifiable in the sink"
        );
    }
}
