//! The diff-sink pipeline counters, end to end through the production fan-out:
//! every mismatch record offered to the writer queue is accounted for exactly
//! once as written or dropped (`enqueued == written + dropped`), which is half
//! of what a campaign verdict calls "drained".
//!
//! Deliberately the only test in this binary: the counters are process-global,
//! so a second test writing mismatches in parallel would make the balance
//! assertion a race.

mod common;

use axum::body::Body;
use axum::http::Request;
use common::{config_from_yaml, metric_value, parts, router, send, wait_until};
use limen::observability::prometheus::{
    self, SinkDropReason, DIFF_SINK_DROPPED_TOTAL, DIFF_SINK_ENQUEUED_TOTAL,
    DIFF_SINK_WRITTEN_TOTAL, SHADOW_IN_FLIGHT,
};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// How many mismatches to drive through the sink.
const MISMATCHES: usize = 3;

/// The `reason`-labelled drop series for one reason.
fn dropped_series(reason: SinkDropReason) -> String {
    format!(
        r#"{DIFF_SINK_DROPPED_TOTAL}{{reason="{}"}}"#,
        reason.as_str()
    )
}

#[tokio::test]
async fn offered_records_balance_against_written_and_dropped() {
    let handle = prometheus::install();
    prometheus::register_verdict_series();

    // Presence at zero, from the real rendered exposition: a verdict tool must
    // be able to tell "no mismatches yet" from "no instrumentation".
    let rendered = handle.render();
    for series in [
        DIFF_SINK_ENQUEUED_TOTAL.to_string(),
        DIFF_SINK_WRITTEN_TOTAL.to_string(),
        SHADOW_IN_FLIGHT.to_string(),
        dropped_series(SinkDropReason::QueueFull),
        dropped_series(SinkDropReason::IoError),
        dropped_series(SinkDropReason::WriterGone),
    ] {
        assert_eq!(
            metric_value(&rendered, &series),
            Some(0.0),
            "{series} must render at zero before any traffic:\n{rendered}"
        );
    }

    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"name":"A"}"#))
        .mount(&legacy)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"name":"B"}"#))
        .mount(&new)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let sink_dir = dir.path().join("diffs");
    // The production builder, so this exercises the real metrics+sink fan-out.
    let app = router(&config_from_yaml(&format!(
        r#"
diff_sink:
  dir: "{}"
routes:
  - id: r
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{}"
    new_upstream: "{}"
    mode: shadow_legacy_primary
    comparison: {{ enabled: true, sample_rate: 1.0, max_body_bytes: 262144 }}
"#,
        sink_dir.display(),
        legacy.uri(),
        new.uri()
    )));

    for i in 0..MISMATCHES {
        let resp = send(
            &app,
            Request::builder()
                .uri(format!("/devices/{i}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(parts(resp).await.0, 200);
    }

    // The writer thread appends off the shadow tasks, so wait on the counter
    // rather than a sleep.
    wait_until("every mismatch to be written", || {
        metric_value(&handle.render(), DIFF_SINK_WRITTEN_TOTAL) == Some(MISMATCHES as f64)
    })
    .await;

    // One snapshot for all three terms, so the equation is read off a single
    // consistent exposition rather than three racing scrapes.
    let rendered = handle.render();
    let enqueued = metric_value(&rendered, DIFF_SINK_ENQUEUED_TOTAL).expect("enqueued series");
    let written = metric_value(&rendered, DIFF_SINK_WRITTEN_TOTAL).expect("written series");
    let dropped: f64 = SinkDropReason::ALL
        .into_iter()
        .map(|reason| {
            metric_value(&rendered, &dropped_series(reason))
                .unwrap_or_else(|| panic!("{reason:?} series"))
        })
        .sum();
    assert_eq!(enqueued, MISMATCHES as f64, "every mismatch was offered");
    assert_eq!(dropped, 0.0, "a healthy sink drops nothing:\n{rendered}");
    assert_eq!(
        enqueued,
        written + dropped,
        "the drain equation must balance:\n{rendered}"
    );

    // …and the records really are on disk, not just counted.
    let report = limen::observability::sink::read_report(
        &sink_dir,
        &limen::observability::sink::ReportFilter::default(),
        3,
    )
    .unwrap();
    assert_eq!(report.total, MISMATCHES);
    assert_eq!(report.malformed_lines, 0);
}
