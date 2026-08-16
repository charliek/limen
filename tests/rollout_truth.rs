//! Rollout and breaker *truth* (plan 016): what `/metrics` says about a rollout
//! has to be what the router is actually doing.
//!
//! The target-percentage gauge and the routing decision are resolved by the
//! same function, so these tests pin the two together — a gauge that agreed
//! with the flag file but not with the traffic would be worse than no gauge at
//! all, because a rollout review would believe it.
//!
//! `prometheus::install()` hands back one process-wide recorder and these tests
//! run in parallel against it, so **every test here uses route ids nobody else
//! in this file uses**. Sharing one would let another test's traffic land on a
//! counter this one asserts an exact value for.

mod common;

use std::path::Path;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use axum::Router;
use common::{config_from_yaml, free_port, parts, send, spawn_proxy, wait_serving};
use limen::config::model::Config;
use limen::health::endpoints::ControlState;
use limen::http::server::{build_state, control_plane_router, data_plane_router};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The resolved rollout target gauge.
const TARGET: &str = "limen_rollout_resolved_target_percentage";
/// The breaker transition counter.
const TRANSITIONS: &str = "limen_breaker_transitions_total";

/// The value of the one exposition line for `name` carrying every label in
/// `labels`, or `None` when the series is absent — a distinction these tests
/// rest on, since pre-registration is precisely the difference between "0" and
/// "no such series".
///
/// Label-order independent on purpose: the assertions pin the series, not the
/// exporter's rendering order.
fn series(rendered: &str, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
    rendered.lines().find_map(|line| {
        let rest = line.strip_prefix(name)?;
        let (label_block, value) = rest.rsplit_once(' ')?;
        labels
            .iter()
            .all(|(k, v)| label_block.contains(&format!(r#"{k}="{v}""#)))
            .then(|| value.trim().parse().ok())
            .flatten()
    })
}

/// The transition counter for one from/to pair, or `None` if unregistered.
fn transition(rendered: &str, route: &str, from: &str, to: &str) -> Option<f64> {
    series(
        rendered,
        TRANSITIONS,
        &[("route", route), ("from", from), ("to", to)],
    )
}

/// Build both planes over one shared state, exactly as `serve_with_shutdown`
/// does: the gauge is only trustworthy if the plane that reports it and the
/// plane that routes read the same state.
fn planes(cfg: &Config) -> (Router, Router) {
    let handle = limen::observability::prometheus::install();
    let state = build_state(cfg, Path::new(".")).expect("build state");
    let data = data_plane_router(state.clone());
    let control = control_plane_router(
        ControlState::new(
            state.flags().clone(),
            state.routes_arc(),
            handle,
            state.fail_safe_mode(),
        ),
        "/metrics",
    );
    (data, control)
}

/// Scrape the control plane's `/metrics`.
async fn scrape(control: &Router) -> String {
    let resp = send(
        control,
        Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let (status, _, body) = parts(resp).await;
    assert_eq!(status, 200, "scrape must succeed");
    body
}

/// A legacy/new pair that each name themselves in `x-upstream`.
async fn upstreams() -> (MockServer, MockServer) {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).insert_header("x-upstream", "legacy"))
        .mount(&legacy)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).insert_header("x-upstream", "new"))
        .mount(&new)
        .await;
    (legacy, new)
}

/// Which upstream served one request, by the header the mocks set.
async fn served_by(app: &Router, tenant: &str) -> String {
    let resp = send(
        app,
        Request::builder()
            .uri("/x")
            .header("x-tenant-id", tenant)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let (_, headers, _) = parts(resp).await;
    headers
        .get("x-upstream")
        .expect("upstream marker")
        .to_str()
        .unwrap()
        .to_string()
}

/// A single `percentage_split` route over a static flag value.
fn static_split(route: &str, legacy: &str, new: &str, flag: Option<f64>, default: f64) -> Config {
    let values = match flag {
        Some(v) => format!("    values: {{ \"migration.{route}.percentage\": {v} }}\n"),
        None => "    values: {}\n".to_string(),
    };
    config_from_yaml(&format!(
        r#"
flags:
  provider: static
  static:
{values}
routes:
  - id: {route}
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{new}"
    mode: percentage_split
    rollout:
      percentage_flag: "migration.{route}.percentage"
      default_percentage: {default}
      assignment_key: {{ header: "x-tenant-id", fallback: request_random }}
"#
    ))
}

/// A `failover_to_legacy` route guarded by a fast-cycling breaker.
fn breaker_config(route: &str, legacy: &str, new: &str, open_ms: u64) -> Config {
    config_from_yaml(&format!(
        r#"
routes:
  - id: {route}
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{new}"
    mode: failover_to_legacy
    circuit_breaker:
      enabled: true
      failure_rate_threshold: 0.5
      min_requests: 2
      open_duration_ms: {open_ms}
      half_open_max_requests: 1
"#
    ))
}

/// Absence≠zero, applied to the rollout surface: a proxy that has served
/// nothing must still say so in numbers. A reviewer reading "the breaker never
/// half-opened" off a missing series is reading the instrumentation's silence,
/// not the system's behavior.
#[tokio::test]
async fn a_fresh_proxy_pre_registers_the_target_gauge_and_every_transition_pair() {
    let (legacy, new) = upstreams().await;
    let cfg = config_from_yaml(&format!(
        r#"
flags:
  provider: static
  static:
    values: {{ "migration.pre-reg.percentage": 40 }}
routes:
  - id: pre-reg
    match: {{ methods: ["GET"], path_prefix: "/split" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{new}"
    mode: percentage_split
    rollout:
      percentage_flag: "migration.pre-reg.percentage"
      default_percentage: 0
      assignment_key: {{ header: "x-tenant-id", fallback: request_random }}
    circuit_breaker:
      enabled: true
      failure_rate_threshold: 0.5
      min_requests: 2
      open_duration_ms: 60000
      half_open_max_requests: 1
  - id: pre-reg-inert
    match: {{ methods: ["GET"], path_prefix: "/inert" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{new}"
    mode: legacy_only
    circuit_breaker:
      enabled: true
      failure_rate_threshold: 0.5
      min_requests: 2
      open_duration_ms: 60000
      half_open_max_requests: 1
"#,
        legacy = legacy.uri(),
        new = new.uri()
    ));
    // No traffic at all between building the proxy and scraping it.
    let (_data, control) = planes(&cfg);
    let body = scrape(&control).await;

    assert_eq!(
        series(&body, TARGET, &[("route", "pre-reg")]),
        Some(40.0),
        "the target gauge renders before any request:\n{body}"
    );
    for (from, to) in [
        ("closed", "open"),
        ("open", "half_open"),
        ("half_open", "closed"),
        ("half_open", "open"),
    ] {
        assert_eq!(
            transition(&body, "pre-reg", from, to),
            Some(0.0),
            "{from}->{to} must render at zero before any traffic:\n{body}"
        );
        // The other half of absence≠zero: `pre-reg-inert` configures a breaker
        // on a `legacy_only` route, where no request ever consults it. Zero
        // there would claim a breaker is being watched and holding — a series
        // that cannot move must not be advertised at all.
        assert_eq!(
            transition(&body, "pre-reg-inert", from, to),
            None,
            "a breaker no mode consults must register no {from}->{to} series:\n{body}"
        );
    }
    assert_eq!(
        series(&body, TARGET, &[("route", "pre-reg-inert")]),
        None,
        "and a route with no rollout has no target to report:\n{body}"
    );
}

/// The gauge under the real serve loop: a bound proxy, a file provider, and a
/// flag edit that nobody explicitly refreshes.
#[tokio::test]
async fn the_target_gauge_follows_the_flag_through_the_real_poll_loop() {
    let (legacy, new) = upstreams().await;
    let dir = tempfile::tempdir().unwrap();
    let flags_path = dir.path().join("flags.yaml");
    std::fs::write(&flags_path, "migration.poll-split.percentage: 25\n").unwrap();

    let data_port = free_port();
    let control_port = free_port();
    let cfg = config_from_yaml(&format!(
        r#"
server:
  listen_addr: "127.0.0.1:{data_port}"
metrics:
  listen_addr: "127.0.0.1:{control_port}"
  path: "/metrics"
flags:
  provider: file
  file: {{ path: "{flags}", refresh_interval_ms: 50 }}
  stale_ttl_ms: 30000
  fail_safe_mode: legacy_only
routes:
  - id: poll-split
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{new}"
    mode: percentage_split
    rollout:
      percentage_flag: "migration.poll-split.percentage"
      default_percentage: 0
      assignment_key: {{ header: "x-tenant-id", fallback: request_random }}
"#,
        flags = flags_path.to_str().unwrap(),
        legacy = legacy.uri(),
        new = new.uri()
    ));

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let (_shutdown, server) = spawn_proxy(cfg);
    wait_serving(&client, &format!("http://127.0.0.1:{data_port}/x")).await;

    let metrics_url = format!("http://127.0.0.1:{control_port}/metrics");
    let read = |client: reqwest::Client, url: String| async move {
        let body = client.get(&url).send().await.unwrap().text().await.unwrap();
        series(&body, TARGET, &[("route", "poll-split")])
    };
    assert_eq!(
        read(client.clone(), metrics_url.clone()).await,
        Some(25.0),
        "the gauge starts at the flag file's value"
    );

    // Move the flag and let the *poll loop* pick it up — no explicit refresh.
    std::fs::write(&flags_path, "migration.poll-split.percentage: 75\n").unwrap();
    let moved = async {
        loop {
            if read(client.clone(), metrics_url.clone()).await == Some(75.0) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), moved)
        .await
        .expect("the gauge must follow the flag file without a restart");

    drop(_shutdown);
    let _ = server.await;
}

/// Stale flags displace the rollout entirely, so the *target* is zero — the
/// staleness gauges next to it are what say why.
#[tokio::test]
async fn stale_flags_render_the_target_at_zero() {
    let (legacy, new) = upstreams().await;
    let dir = tempfile::tempdir().unwrap();
    let flags_path = dir.path().join("flags.yaml");
    // 100% rolled out, but with a 1ms staleness TTL nothing stays fresh.
    std::fs::write(&flags_path, "migration.stale-split.percentage: 100\n").unwrap();
    let cfg = config_from_yaml(&format!(
        r#"
flags:
  provider: file
  file: {{ path: "{flags}", refresh_interval_ms: 60000 }}
  stale_ttl_ms: 1
  fail_safe_mode: legacy_only
routes:
  - id: stale-split
    match: {{ methods: ["GET"], path_prefix: "/" }}
    legacy_upstream: "{legacy}"
    new_upstream: "{new}"
    mode: percentage_split
    rollout:
      percentage_flag: "migration.stale-split.percentage"
      default_percentage: 100
      assignment_key: {{ header: "x-tenant-id", fallback: request_random }}
"#,
        flags = flags_path.to_str().unwrap(),
        legacy = legacy.uri(),
        new = new.uri()
    ));
    let (data, control) = planes(&cfg);
    tokio::time::sleep(Duration::from_millis(20)).await;

    let body = scrape(&control).await;
    assert_eq!(
        series(&body, TARGET, &[("route", "stale-split")]),
        Some(0.0),
        "a stale fail-safe targets nothing at new:\n{body}"
    );
    // And the routing agrees: the gauge is not describing a different world.
    for tenant in ["a", "b", "c"] {
        assert_eq!(served_by(&data, tenant).await, "legacy");
    }
}

#[tokio::test]
async fn a_missing_flag_renders_the_default_percentage() {
    let (legacy, new) = upstreams().await;
    let cfg = static_split("default-split", &legacy.uri(), &new.uri(), None, 30.0);
    let (_data, control) = planes(&cfg);

    let body = scrape(&control).await;
    assert_eq!(
        series(&body, TARGET, &[("route", "default-split")]),
        Some(30.0),
        "an unset flag resolves to the configured default:\n{body}"
    );
}

/// The load-bearing fixture: the gauge and the traffic are two readings of one
/// resolution, so they must never disagree.
#[tokio::test]
async fn the_target_gauge_agrees_with_the_observed_routing_split() {
    let (legacy, new) = upstreams().await;

    let none = static_split("agree-none", &legacy.uri(), &new.uri(), Some(0.0), 0.0);
    let (data, control) = planes(&none);
    let body = scrape(&control).await;
    assert_eq!(series(&body, TARGET, &[("route", "agree-none")]), Some(0.0));
    for tenant in ["a", "b", "c", "d"] {
        assert_eq!(
            served_by(&data, tenant).await,
            "legacy",
            "a 0 gauge must mean nobody is on new"
        );
    }

    let all = static_split("agree-all", &legacy.uri(), &new.uri(), Some(100.0), 0.0);
    let (data, control) = planes(&all);
    let body = scrape(&control).await;
    assert_eq!(
        series(&body, TARGET, &[("route", "agree-all")]),
        Some(100.0)
    );
    for tenant in ["a", "b", "c", "d"] {
        assert_eq!(
            served_by(&data, tenant).await,
            "new",
            "a 100 gauge must mean everybody is on new"
        );
    }
}

/// A full recovery cycle, counted by label: closed→open, open→half_open,
/// half_open→closed. Asserted by value, not by presence — a counter that
/// incremented the wrong pair would still "contain" the right label.
#[tokio::test]
async fn a_recovering_breaker_counts_closed_open_then_half_open_closed() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&legacy)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&new)
        .await;

    let cfg = breaker_config("cb-recover", &legacy.uri(), &new.uri(), 100);
    let (data, control) = planes(&cfg);

    // Two new-side 500s at min_requests=2 open the breaker.
    for _ in 0..2 {
        send(
            &data,
            Request::builder().uri("/x").body(Body::empty()).unwrap(),
        )
        .await;
    }
    let body = scrape(&control).await;
    assert_eq!(
        transition(&body, "cb-recover", "closed", "open"),
        Some(1.0),
        "one open, not one per failure:\n{body}"
    );
    assert_eq!(
        transition(&body, "cb-recover", "open", "half_open"),
        Some(0.0)
    );

    // New recovers, then the open window elapses so the next request is a trial.
    new.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&new)
        .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    send(
        &data,
        Request::builder().uri("/x").body(Body::empty()).unwrap(),
    )
    .await;

    let body = scrape(&control).await;
    assert_eq!(
        transition(&body, "cb-recover", "open", "half_open"),
        Some(1.0),
        "the elapsed open window is a transition, counted once:\n{body}"
    );
    assert_eq!(
        transition(&body, "cb-recover", "half_open", "closed"),
        Some(1.0),
        "one successful trial at half_open_max=1 closes the breaker:\n{body}"
    );
    assert_eq!(
        transition(&body, "cb-recover", "half_open", "open"),
        Some(0.0),
        "nothing reopened:\n{body}"
    );
    assert_eq!(
        transition(&body, "cb-recover", "closed", "open"),
        Some(1.0),
        "still just the one open:\n{body}"
    );
}

/// The fourth pair: a probe that fails sends the breaker straight back to open.
#[tokio::test]
async fn a_failing_probe_counts_half_open_open() {
    let legacy = MockServer::start().await;
    let new = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&legacy)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&new)
        .await;

    let cfg = breaker_config("cb-reopen", &legacy.uri(), &new.uri(), 100);
    let (data, control) = planes(&cfg);

    for _ in 0..2 {
        send(
            &data,
            Request::builder().uri("/x").body(Body::empty()).unwrap(),
        )
        .await;
    }
    // New is still failing when the trial goes out.
    tokio::time::sleep(Duration::from_millis(150)).await;
    send(
        &data,
        Request::builder().uri("/x").body(Body::empty()).unwrap(),
    )
    .await;

    let body = scrape(&control).await;
    assert_eq!(
        transition(&body, "cb-reopen", "closed", "open"),
        Some(1.0),
        "{body}"
    );
    assert_eq!(
        transition(&body, "cb-reopen", "open", "half_open"),
        Some(1.0),
        "{body}"
    );
    assert_eq!(
        transition(&body, "cb-reopen", "half_open", "open"),
        Some(1.0),
        "a failed probe reopens, and says so by label:\n{body}"
    );
    assert_eq!(
        transition(&body, "cb-reopen", "half_open", "closed"),
        Some(0.0),
        "nothing closed:\n{body}"
    );
}
