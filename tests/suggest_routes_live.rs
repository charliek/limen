//! `suggest-routes` against a live control plane: the quiescence poll, and the
//! round trip that makes `--profile` an honest substitute for it.
//!
//! Its own test binary. The quiescence poll reads the process-global
//! `limen_in_flight_requests` gauge, so any concurrently-bound proxy in the
//! same binary would hold it above zero and this test would measure the other
//! test's traffic.

use std::time::Duration;

use limen::config::model::Config;
use limen::draft::{self, ProfileSource, SuggestOptions};
use limen::observability::observe::ObserveProfile;
use limen::suggest::{Disposition, Reason, DEFAULT_MAX_COMPARE_PATHS, DEFAULT_MIN_SAMPLES};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::{free_port, spawn_proxy, wait_serving};

#[tokio::test]
async fn a_live_profile_quiesces_and_round_trips_through_a_file() {
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string("{\"ok\":true}"),
        )
        .mount(&legacy)
        .await;

    let data_port = free_port();
    let control_port = free_port();
    let config: Config = serde_yaml::from_str(&format!(
        r#"
server:
  listen_addr: "127.0.0.1:{data_port}"
metrics:
  listen_addr: "127.0.0.1:{control_port}"
  path: "/metrics"
observe: {{}}
routes:
  - id: reads
    match: {{ methods: ["GET"], path_prefix: "/api" }}
    legacy_upstream: "{legacy}"
    mode: legacy_only
  - id: conversation
    match: {{ methods: ["GET"], path_template: "/conversations/{{id}}" }}
    legacy_upstream: "{legacy}"
    mode: legacy_only
"#,
        legacy = legacy.uri(),
    ))
    .unwrap();

    let (_shutdown_tx, _server) = spawn_proxy(config.clone());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let url = format!("http://127.0.0.1:{data_port}/api/things");
    wait_serving(&client, &url).await;
    // Eight identical reads: enough to clear the default `--min-samples`, and
    // repeated so the stability evidence candidacy requires actually exists.
    for _ in 0..8 {
        assert!(client.get(&url).send().await.unwrap().status().is_success());
    }
    // …and nine conversations, each read twice. Raw, that is nine distinct
    // paths — over the default `--max-compare-paths` of eight, so R7 would
    // demote the route as a wildcard proxy. The template says those nine ids
    // are one operation, and the live recorder counts it that way.
    for n in 0..9 {
        let url = format!("http://127.0.0.1:{data_port}/conversations/c{n}");
        for _ in 0..2 {
            assert!(client.get(&url).send().await.unwrap().status().is_success());
        }
    }

    let opts = SuggestOptions {
        source: ProfileSource::ControlPlane {
            base: format!("http://127.0.0.1:{control_port}"),
            metrics_path: "/metrics".to_string(),
        },
        min_samples: DEFAULT_MIN_SAMPLES,
        max_compare_paths: DEFAULT_MAX_COMPARE_PATHS,
        drain_deadline: Duration::from_millis(5_000),
        poll_interval: Duration::from_millis(50),
    };
    let live = draft::run_suggest_routes(&config, &opts)
        .await
        .expect("the profile should quiesce once traffic has stopped");
    assert_eq!(live.exit_code, 0, "{:?}", live.warnings);
    assert_eq!(live.suggestions.len(), 2);
    assert_eq!(
        live.suggestions[0].disposition,
        Disposition::CompareCandidate
    );
    assert_eq!(live.suggestions[0].reason, Reason::StableRepeatedReads);
    assert!(live.suggestions[0].evidence.reads >= 8);
    assert_eq!(live.suggestions[0].evidence.match_basis, "prefix:/api");

    // The live path's half of the basis contract: the proxy recorded the
    // matcher it was running, and this same config passed the cross-check —
    // which it can only do because both sides compile the route the same way.
    let templated = &live.suggestions[1];
    assert_eq!(templated.route_id, "conversation");
    assert_eq!(
        templated.evidence.match_basis,
        "template:/conversations/{id}"
    );
    assert_eq!(templated.evidence.reads, 18);
    assert_eq!(
        templated.evidence.distinct_read_paths, 1,
        "nine ids, one shape"
    );
    // …and the classification that normalization buys: nine raw paths would
    // have tripped R7's ceiling of eight and demoted the route.
    assert_eq!(templated.disposition, Disposition::CompareCandidate);
    assert_eq!(templated.reason, Reason::StableRepeatedReads);

    // The same document, saved and re-read, must classify identically —
    // otherwise `--profile` would be a different tool wearing the same flag.
    let document = client
        .get(format!("http://127.0.0.1:{control_port}/observe/profile"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("profile.json");
    std::fs::write(&path, &document).unwrap();
    let saved = draft::run_suggest_routes(
        &config,
        &SuggestOptions {
            source: ProfileSource::File(path),
            ..opts.clone()
        },
    )
    .await
    .expect("a saved profile loads");
    assert_eq!(saved.suggestions, live.suggestions);

    // …and the document really is the profile model, not a lookalike.
    let parsed: ObserveProfile = serde_json::from_str(&document).unwrap();
    assert!(parsed.routes.contains_key("reads"));
}

#[tokio::test]
async fn a_proxy_that_is_not_observing_is_input_unavailable() {
    // The endpoint is registered only while observe mode is on, so its 404 is
    // "this proxy has profiled nothing" — an unavailable input (50), never an
    // empty profile.
    let legacy = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&legacy)
        .await;

    let data_port = free_port();
    let control_port = free_port();
    let config: Config = serde_yaml::from_str(&format!(
        r#"
server:
  listen_addr: "127.0.0.1:{data_port}"
metrics:
  listen_addr: "127.0.0.1:{control_port}"
  path: "/metrics"
routes:
  - id: reads
    match: {{ methods: ["GET"], path_prefix: "/api" }}
    legacy_upstream: "{legacy}"
    mode: legacy_only
"#,
        legacy = legacy.uri(),
    ))
    .unwrap();

    let (_shutdown_tx, _server) = spawn_proxy(config.clone());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    wait_serving(&client, &format!("http://127.0.0.1:{data_port}/api/x")).await;

    let err = draft::run_suggest_routes(
        &config,
        &SuggestOptions {
            source: ProfileSource::ControlPlane {
                base: format!("http://127.0.0.1:{control_port}"),
                metrics_path: "/metrics".to_string(),
            },
            min_samples: DEFAULT_MIN_SAMPLES,
            max_compare_paths: DEFAULT_MAX_COMPARE_PATHS,
            drain_deadline: Duration::from_millis(2_000),
            poll_interval: Duration::from_millis(50),
        },
    )
    .await
    .expect_err("a proxy with no observe block cannot serve a profile");
    assert_eq!(err.exit_code(), 50);
    assert!(err.to_string().contains("observe"), "{err}");
}
