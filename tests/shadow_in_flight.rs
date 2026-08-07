//! The `limen_shadow_in_flight` gauge: it must be up for exactly as long as a
//! fire-and-forget shadow task lives, and back at zero afterwards on *every*
//! exit path. A campaign verdict waits on this gauge to decide the pipeline has
//! drained, so a leaked increment would hang the verdict forever and a missing
//! one would declare a still-running shadow finished.
//!
//! Deliberately the only test in this binary: the gauge is process-global, so a
//! second test running in parallel would make "back to zero" a race.

mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use common::{metric_value, parts, router_with_observer, send, shadow_config, wait_until};
use limen::compare::result::ComparisonResult;
use limen::observability::prometheus::{self, SHADOW_IN_FLIGHT};
use limen::observability::{ShadowFailure, ShadowMeta, ShadowObserver, SkipReason};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Counts the terminal observer callbacks, so the test can wait on the shadow's
/// *outcome* rather than on a sleep.
#[derive(Clone, Default)]
struct Capture {
    comparisons: Arc<Mutex<usize>>,
    failures: Arc<Mutex<usize>>,
}

impl ShadowObserver for Capture {
    fn shadow_dispatched(&self, _meta: &ShadowMeta) {}
    fn comparison(&self, _meta: &ShadowMeta, _result: &ComparisonResult) {
        *self.comparisons.lock().unwrap() += 1;
    }
    fn shadow_skipped(&self, _meta: &ShadowMeta, _reason: SkipReason) {}
    fn shadow_failed(&self, _meta: &ShadowMeta, _failure: ShadowFailure) {
        *self.failures.lock().unwrap() += 1;
    }
    fn comparison_skipped(&self, _meta: &ShadowMeta, _reason: SkipReason) {}
}

#[tokio::test]
async fn the_gauge_rises_with_a_shadow_and_returns_to_zero_on_timeout_and_completion() {
    let handle = prometheus::install();
    prometheus::register_verdict_series();
    let gauge = || metric_value(&handle.render(), SHADOW_IN_FLIGHT);
    assert_eq!(gauge(), Some(0.0), "registered at zero before any traffic");

    // --- The timeout path: a shadow that never gets an answer. ---
    let legacy = MockServer::start().await;
    let slow_new = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&legacy)
        .await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("{}")
                .set_delay(Duration::from_secs(30)),
        )
        .mount(&slow_new)
        .await;

    let capture = Capture::default();
    let app = router_with_observer(
        &shadow_config(&legacy.uri(), &slow_new.uri(), 200),
        Arc::new(capture.clone()),
    );
    let resp = send(
        &app,
        Request::builder().uri("/x").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(parts(resp).await.0, 200);

    wait_until("the shadow to be counted in flight", || {
        gauge() == Some(1.0)
    })
    .await;
    wait_until("the shadow to time out", || {
        *capture.failures.lock().unwrap() == 1
    })
    .await;
    wait_until("the gauge to return to zero after a timeout", || {
        gauge() == Some(0.0)
    })
    .await;

    // --- The completion path: a shadow that runs to a comparison. ---
    let new = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&new)
        .await;
    let app = router_with_observer(
        &shadow_config(&legacy.uri(), &new.uri(), 2000),
        Arc::new(capture.clone()),
    );
    let resp = send(
        &app,
        Request::builder().uri("/y").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(parts(resp).await.0, 200);

    wait_until("the comparison to complete", || {
        *capture.comparisons.lock().unwrap() == 1
    })
    .await;
    wait_until("the gauge to return to zero after completion", || {
        gauge() == Some(0.0)
    })
    .await;
}
