//! The drain loop's own decision paths, driven by a stub control plane
//! serving canned expositions. `evaluate` is pure and unit-tested; these are
//! the decisions `run_verdict` makes *before* it — exactly the exit-50/30
//! distinctions campaign wrappers branch on, so their regression must not be
//! silent. No proxy and no global recorder are involved: every test talks to
//! its own wiremock instance, so this binary is parallel-safe.

use std::time::Duration;

use limen::config::model::Config;
use limen::verdict::{self, VerdictOptions};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A minimal config with one compared route (default floor 1).
fn config() -> Config {
    serde_yaml::from_str(
        r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "http://l"
    new_upstream: "http://n"
    mode: shadow_legacy_primary
    comparison: { enabled: true, sample_rate: 1.0 }
"#,
    )
    .expect("test config")
}

fn options(base: &str, sink_dir: &std::path::Path) -> VerdictOptions {
    VerdictOptions {
        sink_dir: sink_dir.to_path_buf(),
        control_base: base.trim_end_matches('/').to_string(),
        metrics_path: "/metrics".to_string(),
        canary: false,
        offline: false,
        drain_deadline: Duration::from_secs(5),
        poll_interval: Duration::from_millis(20),
    }
}

/// The four process-wide required families, quiescent and balanced.
const PROCESS_WIDE: &str = "\
limen_shadow_in_flight 0
limen_diff_sink_enqueued_total 0
limen_diff_sink_written_total 0
limen_diff_sink_dropped_total{reason=\"io_error\"} 0
";

/// Every uncompared series a live limen pre-registers for `route_id`, at zero.
///
/// Not abridged: these three families gate a floored route, so `verdict`
/// requires them *per route and per reason* — one route's skips may not vouch
/// for another's, and a family-level "present somewhere" check is exactly the
/// hole that let an unregistered floored route read as a proven zero.
fn registered_for(route_id: &str) -> String {
    let mut out = String::new();
    for reason in [
        "concurrency_limit",
        "response_too_large",
        "request_too_large",
        "event_stream",
        "response_buffer_timeout",
    ] {
        for family in [
            "limen_shadow_skipped_total",
            "limen_comparison_skipped_total",
        ] {
            out.push_str(&format!(
                "{family}{{route=\"{route_id}\",reason=\"{reason}\"}} 0\n"
            ));
        }
    }
    for reason in ["timeout", "error"] {
        out.push_str(&format!(
            "limen_shadow_failed_total{{route=\"{route_id}\",reason=\"{reason}\"}} 0\n"
        ));
    }
    out
}

/// A balanced, quiescent exposition with `n` comparisons on route `r`.
fn balanced(n: u64) -> String {
    format!(
        "{PROCESS_WIDE}{}limen_comparisons_total{{route=\"r\",result=\"match\"}} {n}\n",
        registered_for("r")
    )
}

async fn stub(bodies: Vec<String>) -> MockServer {
    let server = MockServer::start().await;
    // Serve the canned expositions in order; the last repeats forever.
    let (head, tail) = bodies.split_at(bodies.len() - 1);
    for body in head {
        Mock::given(method("GET"))
            .and(path("/metrics"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body.clone()))
            .up_to_n_times(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/metrics"))
        .respond_with(ResponseTemplate::new(200).set_body_string(tail[0].clone()))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn an_absent_required_series_is_typed_input_unavailable() {
    // No limen_shadow_in_flight at all: an older binary or a renderer
    // regression — never "zero shadows in flight".
    let server = stub(vec!["limen_diff_sink_enqueued_total 0\n\
         limen_diff_sink_written_total 0\n\
         limen_diff_sink_dropped_total{reason=\"io_error\"} 0\n"
        .to_string()])
    .await;
    let sink = tempfile::tempdir().unwrap();
    let err = verdict::run_verdict(&config(), &options(&server.uri(), sink.path()))
        .await
        .expect_err("absent series must be typed");
    assert!(err.0.contains("absent"), "{err}");
}

#[tokio::test]
async fn a_non_integer_watched_count_is_typed_input_unavailable() {
    let mut body = balanced(1);
    body = body.replace(
        "limen_diff_sink_enqueued_total 0",
        "limen_diff_sink_enqueued_total 1.5",
    );
    let server = stub(vec![body]).await;
    let sink = tempfile::tempdir().unwrap();
    let err = verdict::run_verdict(&config(), &options(&server.uri(), sink.path()))
        .await
        .expect_err("non-exact count must be typed");
    assert!(err.0.contains("non-exact"), "{err}");
}

#[tokio::test]
async fn a_live_over_balance_is_sink_integrity_not_timeout() {
    // More written than ever offered: corrupt counters or the wrong process.
    let body = balanced(1).replace(
        "limen_diff_sink_written_total 0",
        "limen_diff_sink_written_total 2",
    );
    let server = stub(vec![body]).await;
    let sink = tempfile::tempdir().unwrap();
    let v = verdict::run_verdict(&config(), &options(&server.uri(), sink.path()))
        .await
        .expect("over-balance is a verdict, not an error");
    assert_eq!(v.exit_code, 30, "{v:?}");
}

#[tokio::test]
async fn one_balanced_scrape_is_not_enough_to_drain() {
    // Scrape 1 and scrape 2 are each balanced but differ (a comparison landed
    // between them); only the identical pair 2/3 may conclude the drain. The
    // request count proves the loop kept polling past the first balance.
    let server = stub(vec![balanced(1), balanced(2)]).await;
    let sink = tempfile::tempdir().unwrap();
    let v = verdict::run_verdict(&config(), &options(&server.uri(), sink.path()))
        .await
        .expect("verdict");
    assert_eq!(v.exit_code, 0, "{v:?}");
    let scrapes = server
        .received_requests()
        .await
        .expect("request recording on")
        .len();
    assert!(
        scrapes >= 3,
        "drain concluded after {scrapes} scrape(s) — a single balanced scrape \
         must never be treated as quiescence"
    );
}

/// The per-route version boundary, end to end — and the exact false green it
/// closes.
///
/// This is what an *older*, lazily-registering limen renders after a long
/// campaign: unfloored route `r` skipped on every reason and had shadows fail,
/// so all three families exist and are fully populated; **floored** route `s`
/// never did, so it has no sample in any of them. Under a family-level
/// required-series check the scrape validates; `Scrape::sum` then answers
/// `Some(0.0)` for `s` in all three families (absence is per family, not per
/// series, because `any` is set before the label filter); `s` is at its floor
/// with `skipped = 0, shadow_failures = 0`, so it is *met*; `r`'s counts are
/// merely informational because `r` is unfloored — and the run exits **0**.
/// A green verdict from a proxy that cannot even produce the evidence the
/// verdict claims to have checked. Requiring the series per route makes it
/// exit 50 instead, naming the route whose zero could not be trusted.
#[tokio::test]
async fn a_floored_route_with_no_registered_skip_series_is_typed_input_unavailable() {
    let config: Config = serde_yaml::from_str(
        r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/r" }
    legacy_upstream: "http://l"
    new_upstream: "http://n"
    mode: shadow_legacy_primary
    comparison: { enabled: true, sample_rate: 1.0, min_comparisons: 0 }
  - id: s
    match: { methods: ["GET"], path_prefix: "/s" }
    legacy_upstream: "http://l"
    new_upstream: "http://n"
    mode: shadow_legacy_primary
    comparison: { enabled: true, sample_rate: 1.0 }
"#,
    )
    .expect("test config");

    // Route `r` carries every uncompared series, non-zero — the only way a
    // lazily-registering proxy exports one at all.
    let body = format!(
        "{PROCESS_WIDE}{}limen_comparisons_total{{route=\"r\",result=\"match\"}} 5\n\
         limen_comparisons_total{{route=\"s\",result=\"match\"}} 5\n",
        registered_for("r").replace("} 0\n", "} 1\n"),
    );
    let server = stub(vec![body]).await;
    let sink = tempfile::tempdir().unwrap();
    let err = verdict::run_verdict(&config, &options(&server.uri(), sink.path()))
        .await
        .expect_err(
            "a floored route whose gating series were never registered must be typed, not met",
        );
    assert!(err.0.contains("limen_shadow_skipped_total"), "{err}");
    assert!(err.0.contains(r#"route="s""#), "{err}");
    assert!(err.0.contains("predates the gate"), "{err}");
}

/// A config with zero routes is valid, and a *current* limen renders none of
/// the three uncompared families for it — there is nothing to register them
/// against. Reading that as "older binary" (exit 50) would hide the older,
/// sharper answer this config already has: it floors nothing, so a verdict over
/// it vouches for nothing (exit 20).
#[tokio::test]
async fn a_config_with_no_routes_reaches_the_floors_failure_not_the_version_boundary() {
    let config: Config = serde_yaml::from_str("routes: []\n").expect("test config");
    assert!(config.routes.is_empty());

    let server = stub(vec![PROCESS_WIDE.to_string()]).await;
    let sink = tempfile::tempdir().unwrap();
    let v = verdict::run_verdict(&config, &options(&server.uri(), sink.path()))
        .await
        .expect("no route means nothing was owed, not a missing instrumentation");
    assert_eq!(v.exit_code, 20, "{v:?}");
    assert!(v.checks.floors.detail.contains("compares nothing"), "{v:?}");
}
