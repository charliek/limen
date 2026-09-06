//! Uncompared sampled work fails a floored route (limen#23, limen#24), proven
//! against a real bound proxy rather than against a synthetic scrape: each of
//! the four ways limen can sample a request and then not compare it is driven
//! through the data plane, and the verdict is taken off the live control plane.
//!
//! Its own test binary, for the reason `tests/verdict.rs` documents: the
//! Prometheus recorder and its counters are process-global and monotonic. The
//! same constraint applies *within* this file, so it is one test function of
//! ordered phases against one server — each phase's expectation accounts for
//! what the phases before it left in the counters.
//!
//! Every phase leaves its route **at or above its floor**, so the verdict's
//! failure is the *undermined* branch (met the count, could not vouch for the
//! traffic) rather than the starvation branch that has always failed. A fixture
//! that starved would pass whether or not skips gate at all.
//!
//! The one shared shadow slot (`shadow_concurrency_limit: 1`) is held open by a
//! gate the test opens on purpose, never by a sleep: "the permit is still held"
//! is asserted off `limen_shadow_in_flight`, so the concurrency phases cannot
//! race.

use std::time::Duration;

use limen::config::model::Config;
use limen::verdict::{self, CheckStatus, VerdictOptions};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::{free_port, raw_upstream, spawn_proxy, wait_serving, write, Gate};

/// The body every upstream agrees on, so every comparison that happens is a
/// match and the sink stays empty (this test is about floors, not mismatches).
const OK_BODY: &str = "ok";

/// 64 bytes — four times the `too-large` route's 16-byte comparison limit.
const BIG_BODY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A complete HTTP/1.1 response for the raw gated upstream. `connection: close`
/// so the handler can drop the socket after one exchange.
fn http_ok(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Fetch the control plane's exposition.
async fn scrape(client: &reqwest::Client, control_port: u16) -> String {
    client
        .get(format!("http://127.0.0.1:{control_port}/metrics"))
        .send()
        .await
        .expect("control plane reachable")
        .text()
        .await
        .expect("exposition body")
}

/// Poll until `series` reads exactly `want`, or fail naming what it read
/// instead. Every wait in this test is on a counter the proxy publishes, never
/// on the clock.
async fn wait_metric(client: &reqwest::Client, control_port: u16, series: &str, want: f64) {
    for _ in 0..500 {
        let rendered = scrape(client, control_port).await;
        if common::metric_value(&rendered, series) == Some(want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let rendered = scrape(client, control_port).await;
    panic!(
        "timed out waiting for `{series}` to read {want}; it reads {:?}\n{rendered}",
        common::metric_value(&rendered, series)
    );
}

/// Assert a series' current value exactly.
async fn assert_metric(client: &reqwest::Client, control_port: u16, series: &str, want: f64) {
    let rendered = scrape(client, control_port).await;
    assert_eq!(
        common::metric_value(&rendered, series),
        Some(want),
        "`{series}` must read {want}:\n{rendered}"
    );
}

fn skipped(family: &str, route: &str, reason: &str) -> String {
    format!(r#"limen_{family}{{route="{route}",reason="{reason}"}}"#)
}

#[tokio::test]
async fn every_uncompared_reason_fails_its_floored_route() {
    // The shared upstream both legs of most routes point at: `ok` everywhere
    // except the one path that answers over the `too-large` route's limit.
    let same = MockServer::start().await;
    Mock::given(path("/too-large/big"))
        .respond_with(ResponseTemplate::new(200).set_body_string(BIG_BODY))
        .mount(&same)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(OK_BODY))
        .mount(&same)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(OK_BODY))
        .mount(&same)
        .await;

    // The `slow` route's new upstream: it answers `/slow/free` at once and
    // parks on `/slow/held` until this test opens the gate. That parked shadow
    // is what holds the single concurrency permit while the phases below prove
    // both refusal paths.
    let gate = Gate::new();
    let held = gate.clone();
    let gated = raw_upstream(move |mut sock, head| {
        let held = held.clone();
        async move {
            if head.contains("/slow/held") {
                held.wait().await;
            }
            write(&mut sock, &http_ok(OK_BODY)).await;
        }
    })
    .await;

    // The `late` route's new upstream: `/late/fast` answers inside the route's
    // 300ms shadow budget, `/late/slow` cannot.
    let late = MockServer::start().await;
    Mock::given(path("/late/slow"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(OK_BODY)
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&late)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(OK_BODY))
        .mount(&late)
        .await;

    let sink_root = tempfile::tempdir().unwrap();
    let sink_dir = sink_root.path().join("diffs");
    std::fs::create_dir_all(&sink_dir).unwrap();

    let data_port = free_port();
    let control_port = free_port();
    let config: Config = serde_yaml::from_str(&format!(
        r#"
server:
  listen_addr: "127.0.0.1:{data_port}"
  graceful_shutdown_timeout_ms: 2000
  request_body_limit_bytes: 1048576
  shadow_concurrency_limit: 1
metrics:
  listen_addr: "127.0.0.1:{control_port}"
  path: "/metrics"
diff_sink:
  dir: "{sink}"
routes:
  - id: too-large
    match: {{ methods: ["GET"], path_prefix: "/too-large" }}
    legacy_upstream: "{same}"
    new_upstream: "{same}"
    mode: shadow_legacy_primary
    timeouts: {{ primary_ms: 2000, shadow_ms: 2000 }}
    comparison: {{ enabled: true, sample_rate: 1.0, max_body_bytes: 16 }}
  - id: slow
    match: {{ methods: ["GET"], path_prefix: "/slow" }}
    legacy_upstream: "{same}"
    new_upstream: "{gated}"
    mode: shadow_legacy_primary
    timeouts: {{ primary_ms: 2000, shadow_ms: 30000 }}
    comparison: {{ enabled: true, sample_rate: 1.0, max_body_bytes: 262144 }}
  - id: write
    match: {{ methods: ["POST"], path_prefix: "/write" }}
    legacy_upstream: "{same}"
    new_upstream: "{same}"
    mode: shadow_legacy_primary
    timeouts: {{ primary_ms: 2000, shadow_ms: 2000 }}
    comparison:
      enabled: true
      sample_rate: 1.0
      max_body_bytes: 262144
      shadow_methods: ["POST"]
  - id: late
    match: {{ methods: ["GET"], path_prefix: "/late" }}
    legacy_upstream: "{same}"
    new_upstream: "{late}"
    mode: shadow_legacy_primary
    timeouts: {{ primary_ms: 2000, shadow_ms: 300 }}
    comparison: {{ enabled: true, sample_rate: 1.0, max_body_bytes: 262144 }}
"#,
        sink = sink_dir.display(),
        same = same.uri(),
        gated = gated,
        late = late.uri(),
    ))
    .expect("valid test config");

    let (_shutdown_tx, _server) = spawn_proxy(config.clone());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let base = format!("http://127.0.0.1:{data_port}");
    // The readiness probe rides the `too-large` route's small path, so it also
    // puts that route on its floor.
    wait_serving(&client, &format!("{base}/too-large/small")).await;
    wait_metric(&client, control_port, "limen_shadow_in_flight", 0.0).await;

    // Every gating series renders before any of it has happened: this is the
    // pre-registration `verdict::REQUIRED_SERIES` now depends on, asserted off
    // the real exposition rather than off the registration function.
    for route in ["too-large", "slow", "write", "late"] {
        for reason in [
            "concurrency_limit",
            "response_too_large",
            "request_too_large",
            "event_stream",
            "response_buffer_timeout",
        ] {
            for family in ["shadow_skipped_total", "comparison_skipped_total"] {
                let rendered = scrape(&client, control_port).await;
                assert!(
                    common::metric_value(&rendered, &skipped(family, route, reason)).is_some(),
                    "{family}{{route={route},reason={reason}}} must render before it happens"
                );
            }
        }
        for reason in ["timeout", "error"] {
            let rendered = scrape(&client, control_port).await;
            assert!(
                common::metric_value(&rendered, &skipped("shadow_failed_total", route, reason))
                    .is_some(),
                "shadow_failed_total{{route={route},reason={reason}}} must render before it happens"
            );
        }
    }

    // --- Phase (i): a sampled response over `max_body_bytes` ---------------
    // The route is already at its floor from the readiness probe, so this skip
    // *undermines* it rather than starving it.
    assert_eq!(
        client
            .get(format!("{base}/too-large/big"))
            .send()
            .await
            .unwrap()
            .status(),
        200,
        "a demotion costs the comparison, never the client's response"
    );
    wait_metric(
        &client,
        control_port,
        &skipped(
            "comparison_skipped_total",
            "too-large",
            "response_too_large",
        ),
        1.0,
    )
    .await;

    // --- Phase (ii)/(iii): the two concurrency refusals --------------------
    // Warm-ups first, so `slow` and `write` are on their floors before either
    // is refused a slot — one at a time, waiting for the slot back each time.
    // With a single permit, overlapping the warm-ups would refuse one of them
    // and the phases below would then be asserting on a skip they caused by
    // accident.
    assert_eq!(
        client
            .post(format!("{base}/write/one"))
            .body("x")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    wait_metric(
        &client,
        control_port,
        r#"limen_shadow_requests_total{route="write"}"#,
        1.0,
    )
    .await;
    wait_metric(&client, control_port, "limen_shadow_in_flight", 0.0).await;
    assert_metric(
        &client,
        control_port,
        &skipped("shadow_skipped_total", "write", "concurrency_limit"),
        0.0,
    )
    .await;

    assert_eq!(
        client
            .get(format!("{base}/slow/free"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    wait_metric(&client, control_port, "limen_shadow_in_flight", 0.0).await;

    // Two concurrent GETs against the gated upstream and one permit: whichever
    // reaches `try_acquire` first parks on the gate holding it, and the other
    // is refused there — the authoritative refusal path.
    let (a, b) = tokio::join!(
        client.get(format!("{base}/slow/held")).send(),
        client.get(format!("{base}/slow/held")).send()
    );
    assert_eq!(a.unwrap().status(), 200);
    assert_eq!(b.unwrap().status(), 200);
    wait_metric(
        &client,
        control_port,
        &skipped("shadow_skipped_total", "slow", "concurrency_limit"),
        1.0,
    )
    .await;
    // The winner is still parked, so the slot is genuinely occupied for what
    // comes next — asserted, not assumed.
    assert_metric(&client, control_port, "limen_shadow_in_flight", 1.0).await;

    // The *pre-buffer* refusal: an opted-in write reaching `prepare_request_body`
    // while the limiter is saturated is skipped before its body is buffered, so
    // no shadow is dispatched and no memory is spent on one that would be
    // refused anyway.
    assert_eq!(
        client
            .post(format!("{base}/write/two"))
            .body("y")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    wait_metric(
        &client,
        control_port,
        &skipped("shadow_skipped_total", "write", "concurrency_limit"),
        1.0,
    )
    .await;
    assert_metric(
        &client,
        control_port,
        r#"limen_shadow_requests_total{route="write"}"#,
        1.0,
    )
    .await;

    // Release the parked shadow; its comparison completes and the slot frees.
    gate.open();
    wait_metric(&client, control_port, "limen_shadow_in_flight", 0.0).await;
    wait_metric(
        &client,
        control_port,
        r#"limen_comparisons_total{route="slow",result="match"}"#,
        2.0,
    )
    .await;

    // --- Phase (iv): a shadow the new upstream never answers in time -------
    assert_eq!(
        client
            .get(format!("{base}/late/fast"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    wait_metric(
        &client,
        control_port,
        r#"limen_comparisons_total{route="late",result="match"}"#,
        1.0,
    )
    .await;
    assert_eq!(
        client
            .get(format!("{base}/late/slow"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    wait_metric(
        &client,
        control_port,
        &skipped("shadow_failed_total", "late", "timeout"),
        1.0,
    )
    .await;

    // --- The verdict ------------------------------------------------------
    let v = verdict::run_verdict(
        &config,
        &VerdictOptions {
            sink_dir: sink_dir.clone(),
            control_base: format!("http://127.0.0.1:{control_port}"),
            metrics_path: "/metrics".to_string(),
            canary: false,
            offline: false,
            drain_deadline: Duration::from_secs(15),
            poll_interval: Duration::from_millis(50),
        },
    )
    .await
    .expect("the verdict's inputs are all available");

    assert_eq!(v.exit_code, 20, "{v:?}");
    assert_eq!(v.verdict, "floors-unmet");
    assert_eq!(v.checks.floors.status, CheckStatus::Fail);
    assert_eq!(
        v.checks.drain.status,
        CheckStatus::Pass,
        "the gate was opened, so the pipeline must quiesce: {v:?}"
    );
    assert_eq!(v.checks.sink_integrity.status, CheckStatus::Pass, "{v:?}");
    assert_eq!(v.mismatches_total, 0, "every comparison that ran matched");

    // Every floored route met its count and still failed: this is the
    // undermined branch, which is the whole point of the change.
    for row in &v.floors {
        assert!(
            row.floor_met,
            "route {} must reach its floor, or this test proves only starvation: {row:?}",
            row.route_id
        );
        assert!(
            !row.met,
            "route {} left work uncompared: {row:?}",
            row.route_id
        );
    }

    let detail = &v.checks.floors.detail;
    assert!(detail.contains("undermined — at their floor"), "{detail}");
    for (route, reason, knob) in [
        (
            "too-large",
            "response_too_large",
            "comparison.max_body_bytes",
        ),
        (
            "slow",
            "concurrency_limit",
            "server.shadow_concurrency_limit",
        ),
        (
            "write",
            "concurrency_limit",
            "server.shadow_concurrency_limit",
        ),
        ("late", "timeout", "a finding about `new`"),
    ] {
        assert!(detail.contains(route), "{route} missing from:\n{detail}");
        assert!(detail.contains(reason), "{reason} missing from:\n{detail}");
        assert!(detail.contains(knob), "{knob} missing from:\n{detail}");
    }

    // A floored route's counts gate in its own row and are not repeated as
    // "inspected, not gating".
    assert!(
        v.informational.is_empty(),
        "every route here is floored, so nothing is informational: {:?}",
        v.informational
    );
}
