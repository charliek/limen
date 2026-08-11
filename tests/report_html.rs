//! `limen report --format html`: the falsification set.
//!
//! The page's defining property is negative — it must be *unable* to render a
//! failure or a missing input as success — so most of what follows is an
//! attempt to make it do exactly that: withhold an input, hand it a torn one,
//! hand it two that disagree, hand it a verdict that contradicts itself. Each
//! test asserts both the banner state and the state of the section that should
//! have caught it, because a banner that is red for the wrong reason is a
//! banner that will be green for the wrong reason next time.
//!
//! One positive control balances the set: an all-valid fixture that must reach
//! CLEAN, and must be the only page here that says so.

use std::path::Path;
use std::process::{Command, Output};

use limen::report_html::{
    analyze, render, BannerState, FloorClass, Inputs, PageModel, Section, SinkState,
    VerdictArtifact,
};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// One sink line, written by hand so the fixture pins the on-disk format
/// independently of the writer (the `tests/report.rs` builder, which lives in
/// its own test binary and so cannot be imported here).
fn line(timestamp: &str, route: &str, request_id: &str, kinds: &[&str]) -> String {
    serde_json::json!({
        "timestamp": timestamp,
        "route_id": route,
        "request_id": request_id,
        "method": "GET",
        "path": format!("/{route}/1"),
        "mismatch_kinds": kinds,
    })
    .to_string()
}

/// A config with one compared, floored route (`a`) and one exempt (`b`).
const CONFIG: &str = r#"
routes:
  - id: a
    match: { methods: ["GET"], path_prefix: "/a" }
    legacy_upstream: "http://legacy.invalid"
    new_upstream: "http://new.invalid"
    mode: shadow_legacy_primary
    comparison: { enabled: true, sample_rate: 1.0 }
  - id: b
    match: { methods: ["GET"], path_prefix: "/b" }
    legacy_upstream: "http://legacy.invalid"
    mode: legacy_only
"#;

/// A scrape carrying every family the runtime-counters section requires.
const METRICS: &str = "\
limen_comparisons_total{route=\"a\",result=\"match\"} 3
limen_comparison_skipped_total{route=\"a\",reason=\"event_stream\"} 2
limen_comparison_skipped_total{route=\"a\",reason=\"response_buffer_timeout\"} 1
limen_shadow_requests_total{route=\"a\"} 3
limen_shadow_failed_total{route=\"a\",reason=\"timeout\"} 0
";

/// A coherent, clean, online verdict over [`CONFIG`].
fn clean_verdict() -> serde_json::Value {
    serde_json::json!({
        "mode": "online",
        "verdict": "clean",
        "exit_code": 0,
        "checks": {
            "drain": {"status": "pass", "detail": "pipeline quiesced"},
            "floors": {"status": "pass", "detail": "1 floored route(s) all at/above floor"},
            "sink_integrity": {"status": "pass", "detail": "sink and engine counters agree"},
            "canary": {"status": "skipped", "detail": "--canary not requested"},
            "mismatches": {"status": "pass", "detail": "zero non-canary mismatches recorded"},
        },
        "mismatches_total": 0,
        "canary_records": 0,
        "floors": [{"route_id": "a", "comparisons": 3, "floor": 1, "met": true}],
        "sink_mismatches_by_route": {},
        "informational": [],
    })
}

/// A profile over the same route table.
fn profile() -> serde_json::Value {
    serde_json::json!({
        "sample_rate": 1.0,
        "routes": {
            "a": {
                "observations": 3,
                "reads": 3,
                "writes": 0,
                "transport_errors": 0,
                "methods": {"GET": 3},
                "query_names": [],
                "query_names_overflow": false,
                "distinct_read_paths": 1,
                "distinct_read_paths_overflow": false,
                "status_classes": {"2xx": 3},
                "content_types": ["application/json"],
                "content_types_overflow": false,
                "set_cookie_reads": 0,
                "redirect_reads": 0,
                "location_reads": 0,
                "length_repeats": 2,
                "length_varied": 0,
                "length_missing": 0,
                "fingerprint_overflow": false,
            }
        }
    })
}

/// A workspace under construction: files land in a tempdir and the inputs are
/// assembled from whichever ones a test chose to write.
struct Workspace {
    dir: TempDir,
    sink_dir: std::path::PathBuf,
    config: Option<std::path::PathBuf>,
    verdict: Option<std::path::PathBuf>,
    profile: Option<std::path::PathBuf>,
    metrics: Option<std::path::PathBuf>,
}

impl Workspace {
    /// A workspace with a readable, existing-but-empty sink file and nothing
    /// else. Every fixture below adds to this.
    fn new() -> Workspace {
        let dir = tempfile::tempdir().expect("tempdir");
        let sink_dir = dir.path().join("diffs");
        std::fs::create_dir(&sink_dir).expect("sink dir");
        std::fs::write(sink_dir.join("mismatches-2026-08-01.jsonl"), "").expect("sink file");
        Workspace {
            dir,
            sink_dir,
            config: None,
            verdict: None,
            profile: None,
            metrics: None,
        }
    }

    fn write(&self, name: &str, contents: &str) -> std::path::PathBuf {
        let path = self.dir.path().join(name);
        std::fs::write(&path, contents).expect("write fixture");
        path
    }

    fn with_config(mut self, yaml: &str) -> Workspace {
        self.config = Some(self.write("limen.config.yaml", yaml));
        self
    }

    fn with_verdict(mut self, value: &serde_json::Value) -> Workspace {
        self.verdict = Some(self.write("verdict.json", &value.to_string()));
        self
    }

    fn with_raw_verdict(mut self, text: &str) -> Workspace {
        self.verdict = Some(self.write("verdict.json", text));
        self
    }

    fn with_profile(mut self, text: &str) -> Workspace {
        self.profile = Some(self.write("profile.json", text));
        self
    }

    fn with_metrics(mut self, text: &str) -> Workspace {
        self.metrics = Some(self.write("metrics.txt", text));
        self
    }

    /// Replace the sink directory's contents with these lines.
    fn with_sink_lines(self, lines: &[String]) -> Workspace {
        std::fs::write(
            self.sink_dir.join("mismatches-2026-08-01.jsonl"),
            format!("{}\n", lines.join("\n")),
        )
        .expect("sink file");
        self
    }

    /// Point at a sink directory that does not exist.
    fn without_sink_dir(mut self) -> Workspace {
        self.sink_dir = self.dir.path().join("no-such-directory");
        self
    }

    fn inputs(&self) -> Inputs {
        Inputs {
            sink_dir: self.sink_dir.clone(),
            config: self.config.clone(),
            verdict: self.verdict.clone(),
            profile: self.profile.clone(),
            metrics: self.metrics.clone(),
        }
    }

    fn model(&self) -> PageModel {
        analyze(&self.inputs())
    }

    fn page(&self) -> String {
        render(&self.model())
    }
}

/// Everything present and agreeing: the fixture the whole set is calibrated
/// against.
fn canonical() -> Workspace {
    Workspace::new()
        .with_config(CONFIG)
        .with_verdict(&clean_verdict())
        .with_profile(&profile().to_string())
        .with_metrics(METRICS)
}

/// The failure reasons, joined, for a readable assertion message.
fn why(model: &PageModel) -> String {
    format!(
        "failures={:?} incomplete={:?}",
        model.banner.failures, model.banner.incomplete
    )
}

/// Assert the banner is FAILURE and some reason mentions `needle`.
fn assert_failure_naming(model: &PageModel, needle: &str) {
    assert_eq!(model.banner.state, BannerState::Failure, "{}", why(model));
    assert!(
        model.banner.failures.iter().any(|f| f.contains(needle)),
        "no failure mentions {needle:?}: {}",
        why(model)
    );
}

// ---------------------------------------------------------------------------
// The positive control
// ---------------------------------------------------------------------------

#[test]
fn the_canonical_fixture_is_the_one_clean_page() {
    let ws = canonical();
    let model = ws.model();
    assert_eq!(model.banner.state, BannerState::Clean, "{}", why(&model));
    assert!(model.banner.failures.is_empty(), "{}", why(&model));
    assert!(model.banner.incomplete.is_empty(), "{}", why(&model));
    assert!(
        model.evidence.drift.is_empty(),
        "{:?}",
        model.evidence.drift
    );
    assert!(model.evidence.verdict_violations.is_empty());
    // Files exist, hold nothing, and a clean online verdict vouches for them.
    assert_eq!(model.evidence.sink_state, SinkState::VerifiedZero);
    let route_a = model.routes.iter().find(|r| r.id == "a").expect("route a");
    assert_eq!(route_a.floor_class, FloorClass::Met);
    let route_b = model.routes.iter().find(|r| r.id == "b").expect("route b");
    assert_eq!(
        route_b.floor_class,
        FloorClass::NotApplicable,
        "an uncompared route is not owed a floors row"
    );

    let html = ws.page();
    assert_eq!(
        html.matches("CLEAN").count(),
        1,
        "exactly one page-level claim of success"
    );
    assert!(html.contains("event_stream"), "the L1 skip reasons surface");
    assert!(html.contains("response_buffer_timeout"));
    // The five gating checks are on the page with their own details, and the
    // canary count rides the check that would have used it.
    assert!(
        html.contains(
            "<td>drain</td><td><span class=\"pill good\">PASS</span></td>\
                       <td>pipeline quiesced</td>"
        ),
        "{html}"
    );
    assert!(html.contains("--canary not requested — 0 canary record(s) counted"));
    // A profile route's transport errors are shown: a route that could not
    // reach an upstream is not a route that observed nothing.
    assert!(html.contains("Transport errors"), "{html}");
}

#[test]
fn the_page_is_self_contained() {
    for html in [canonical().page(), Workspace::new().page()] {
        for forbidden in [
            "<script", "src=", "href=", "@import", "url(", "<iframe", "<link", " onload",
            " onerror", "http://", "https://",
        ] {
            assert!(!html.contains(forbidden), "page contains {forbidden:?}");
        }
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<style>"));
    }
}

// ---------------------------------------------------------------------------
// Missing required inputs: INCOMPLETE, never clean
// ---------------------------------------------------------------------------

#[test]
fn a_missing_verdict_is_incomplete() {
    let ws = Workspace::new().with_config(CONFIG).with_metrics(METRICS);
    let model = ws.model();
    assert_eq!(
        model.banner.state,
        BannerState::Incomplete,
        "{}",
        why(&model)
    );
    assert!(
        model
            .banner
            .incomplete
            .iter()
            .any(|r| r.contains("--verdict")),
        "{}",
        why(&model)
    );
    assert_eq!(model.evidence.verdict, Section::NotProvided);
    assert!(ws.page().contains("NOT A CLEAN RESULT"));
}

#[test]
fn a_missing_config_is_incomplete() {
    let ws = Workspace::new().with_verdict(&clean_verdict());
    let model = ws.model();
    assert_eq!(
        model.banner.state,
        BannerState::Incomplete,
        "{}",
        why(&model)
    );
    assert!(
        model
            .banner
            .incomplete
            .iter()
            .any(|r| r.contains("--config")),
        "{}",
        why(&model)
    );
    assert_eq!(model.evidence.config, Section::NotProvided);
    // Without a config nothing can be classified against a floor.
    assert!(model
        .routes
        .iter()
        .all(|r| r.floor_class == FloorClass::Undetermined));
}

#[test]
fn a_missing_sink_directory_is_incomplete_not_a_process_failure() {
    let ws = canonical().without_sink_dir();
    let model = ws.model();
    assert_eq!(
        model.banner.state,
        BannerState::Incomplete,
        "{}",
        why(&model)
    );
    assert!(matches!(
        model.evidence.sink_state,
        SinkState::Unavailable(_)
    ));
    assert!(model.evidence.sink.is_unavailable());
    // …and the page still exists, saying so.
    assert!(ws.page().contains("UNAVAILABLE"));
}

#[test]
fn a_sink_with_no_files_is_incomplete() {
    let ws = canonical();
    std::fs::remove_file(ws.sink_dir.join("mismatches-2026-08-01.jsonl")).unwrap();
    let model = ws.model();
    assert_eq!(model.evidence.sink_state, SinkState::NoFiles);
    assert_eq!(
        model.banner.state,
        BannerState::Incomplete,
        "{}",
        why(&model)
    );
    assert!(
        model
            .banner
            .incomplete
            .iter()
            .any(|r| r.contains("no sink files")),
        "{}",
        why(&model)
    );
}

#[test]
fn an_empty_sink_without_a_verdict_is_not_clean() {
    // Files exist and hold nothing — but nothing vouches for the pipeline that
    // wrote them, so the zero proves nothing.
    let ws = Workspace::new().with_config(CONFIG);
    let model = ws.model();
    assert_eq!(model.evidence.sink_state, SinkState::UnverifiedZero);
    assert_ne!(model.banner.state, BannerState::Clean, "{}", why(&model));
    assert!(ws.page().contains("ZERO MISMATCHES (UNVERIFIED)"));
}

#[test]
fn an_empty_sink_with_a_clean_verdict_is_clean() {
    let model = canonical().model();
    assert_eq!(model.evidence.sink_state, SinkState::VerifiedZero);
    assert_eq!(model.banner.state, BannerState::Clean, "{}", why(&model));
}

// ---------------------------------------------------------------------------
// Provided-but-bad optional inputs: FAILURE, never a zero
// ---------------------------------------------------------------------------

#[test]
fn an_empty_metrics_file_is_unavailable_not_zeros() {
    let ws = canonical().with_metrics("");
    let model = ws.model();
    assert!(model.evidence.metrics.is_unavailable());
    assert_failure_naming(&model, "metrics");
    // The page is still produced, and says the counters are unknown.
    assert!(ws.page().contains("UNAVAILABLE"));
}

#[test]
fn a_comment_only_metrics_file_is_unavailable() {
    let ws = canonical().with_metrics("# HELP limen_comparisons_total ...\n# TYPE x counter\n");
    let model = ws.model();
    assert!(model.evidence.metrics.is_unavailable());
    assert_eq!(model.banner.state, BannerState::Failure, "{}", why(&model));
}

#[test]
fn a_metrics_file_missing_a_required_family_is_unavailable() {
    let ws = canonical().with_metrics(
        "limen_comparisons_total{route=\"a\",result=\"match\"} 3\n\
         limen_shadow_requests_total{route=\"a\"} 3\n\
         limen_shadow_failed_total{route=\"a\",reason=\"timeout\"} 0\n",
    );
    let model = ws.model();
    match &model.evidence.metrics {
        Section::Unavailable(why) => {
            assert!(why.contains("limen_comparison_skipped_total"), "{why}");
            assert!(why.contains("not a zero"), "{why}");
        }
        other => panic!("expected unavailable, got {other:?}"),
    }
    assert_eq!(model.banner.state, BannerState::Failure, "{}", why(&model));
}

#[test]
fn a_truncated_profile_is_unavailable_and_not_clean() {
    let full = profile().to_string();
    let ws = canonical().with_profile(&full[..full.len() / 2]);
    let model = ws.model();
    assert!(model.evidence.profile.is_unavailable());
    assert_failure_naming(&model, "profile");
}

// ---------------------------------------------------------------------------
// The verdict artifact
// ---------------------------------------------------------------------------

#[test]
fn a_nonzero_verdict_fails_and_names_its_typed_outcome() {
    for (name, code) in [
        ("mismatches-found", 10),
        ("floors-unmet", 20),
        ("sink-integrity-failure", 30),
        ("drain-timeout", 40),
    ] {
        let mut v = clean_verdict();
        v["verdict"] = serde_json::json!(name);
        v["exit_code"] = serde_json::json!(code);
        // Keep the document self-consistent so *only* the typed code is what
        // the banner reacts to.
        match name {
            "mismatches-found" => {
                v["mismatches_total"] = serde_json::json!(2);
                v["checks"]["mismatches"] = serde_json::json!({"status": "fail", "detail": "2"});
                v["sink_mismatches_by_route"] = serde_json::json!({"a": 2});
            }
            "floors-unmet" => {
                v["floors"] = serde_json::json!([
                    {"route_id": "a", "comparisons": 0, "floor": 1, "met": false}
                ]);
                v["checks"]["floors"] = serde_json::json!({"status": "fail", "detail": "a"});
            }
            "sink-integrity-failure" => {
                v["checks"]["sink_integrity"] =
                    serde_json::json!({"status": "fail", "detail": "dropped"});
            }
            _ => v["checks"]["drain"] = serde_json::json!({"status": "fail", "detail": "timeout"}),
        }
        let ws = match name {
            // The sink must agree with the verdict, or the drift check fires
            // first and the assertion below would pass for the wrong reason.
            "mismatches-found" => canonical().with_verdict(&v).with_sink_lines(&[
                line("2026-08-01T10:00:00Z", "a", "req-1", &["body"]),
                line("2026-08-01T10:00:01Z", "a", "req-2", &["status"]),
            ]),
            _ => canonical().with_verdict(&v),
        };
        let model = ws.model();
        assert_failure_naming(&model, name);
        assert!(
            model
                .banner
                .failures
                .iter()
                .any(|f| f.contains(&format!("exit {code}"))),
            "{name}: {}",
            why(&model)
        );
        // Every one of these outcomes has a failed check behind it, and the
        // checks block must name it rather than leave the exit code to speak
        // for the whole document.
        assert!(
            ws.page().contains("<span class=\"pill bad\">FAIL</span>"),
            "{name}: no check rendered as failed"
        );
    }
}

#[test]
fn the_exit_50_shape_is_read_through_its_discriminator() {
    let ws = canonical().with_verdict(&serde_json::json!({
        "mode": "unavailable",
        "verdict": "input-unavailable",
        "exit_code": 50,
        "error": "the sink directory could not be read",
    }));
    let model = ws.model();
    // Parsed as the ad-hoc shape, not shoehorned into the full one.
    assert!(matches!(
        model.evidence.verdict,
        Section::Ok(VerdictArtifact::InputUnavailable(_))
    ));
    assert_failure_naming(&model, "input-unavailable");
    assert_failure_naming(&model, "exit 50");
    // …and it must not have been mistaken for a verdict that found nothing.
    assert_ne!(model.evidence.sink_state, SinkState::VerifiedZero);
    // The checks block says no check was taken rather than drawing five empty
    // rows, which would read as five checks that found nothing.
    let html = ws.page();
    assert!(html.contains("NO CHECKS TAKEN"), "{html}");
    assert!(!html.contains("PASS"), "{html}");
}

#[test]
fn an_offline_verdict_is_never_clean() {
    let mut v = clean_verdict();
    v["mode"] = serde_json::json!("offline");
    v["floors"] = serde_json::json!([]);
    for check in ["drain", "floors", "sink_integrity", "canary"] {
        v["checks"][check] = serde_json::json!({"status": "skipped", "detail": "offline mode"});
    }
    let ws = canonical().with_verdict(&v);
    let model = ws.model();
    assert_failure_naming(&model, "offline");
    assert_ne!(model.evidence.sink_state, SinkState::VerifiedZero);
    // The skipped checks are shown rather than swallowed: each of the four
    // reaches the page as a SKIPPED row carrying its own detail.
    let html = ws.page();
    for check in ["drain", "floors", "sink integrity", "canary"] {
        assert!(
            html.contains(&format!(
                "<td>{check}</td><td><span class=\"pill warn\">SKIPPED</span></td>\
                 <td>offline mode"
            )),
            "the {check} check is not rendered as skipped: {html}"
        );
    }
}

#[test]
fn a_verdict_that_contradicts_itself_fails() {
    // Exit 0 with a check that failed.
    let mut v = clean_verdict();
    v["checks"]["sink_integrity"] = serde_json::json!({"status": "fail", "detail": "dropped"});
    let ws = canonical().with_verdict(&v);
    let model = ws.model();
    assert_failure_naming(&model, "inconsistent verdict artifact");
    assert_failure_naming(&model, "failed sink integrity check");
    // …and the checks block colors the row from the check's own status, so an
    // exit_code of 0 cannot paint a failed check green.
    assert!(
        ws.page().contains(
            "<td>sink integrity</td><td><span class=\"pill bad\">FAIL</span></td>\
             <td>dropped</td>"
        ),
        "{}",
        ws.page()
    );

    // A met flag that its own counts do not support.
    let mut v = clean_verdict();
    v["floors"] = serde_json::json!([
        {"route_id": "a", "comparisons": 0, "floor": 1, "met": true}
    ]);
    let model = canonical().with_verdict(&v).model();
    assert_failure_naming(&model, "claims met=true");
    assert_ne!(model.evidence.sink_state, SinkState::VerifiedZero);
}

#[test]
fn a_verdict_that_is_not_a_verdict_document_is_unavailable() {
    let model = canonical()
        .with_raw_verdict("{\"hello\": \"world\"}")
        .model();
    assert!(model.evidence.verdict.is_unavailable());
    assert_failure_naming(&model, "verdict");

    let model = canonical().with_raw_verdict("{\"mode\": \"onl").model();
    assert!(model.evidence.verdict.is_unavailable());
}

/// A verdict carrying *one* check is not a verdict carrying five. Defaulted
/// check members would have filled the other four in with an empty status —
/// which is not `"fail"`, and so passes every contradiction test while
/// standing for nothing.
#[test]
fn a_verdict_missing_checks_is_not_a_verdict() {
    let mut v = clean_verdict();
    v["checks"] = serde_json::json!({
        "sink_integrity": {"status": "pass", "detail": "sink and engine counters agree"}
    });
    let ws = canonical().with_verdict(&v);
    let model = ws.model();
    assert!(
        model.evidence.verdict.is_unavailable(),
        "four absent checks parsed: {:?}",
        model.evidence.verdict
    );
    assert_ne!(model.banner.state, BannerState::Clean, "{}", why(&model));
    assert_ne!(
        model.evidence.sink_state,
        SinkState::VerifiedZero,
        "an unreadable verdict vouched for an empty sink"
    );

    // A check missing only its `detail` is just as unreadable — a check with
    // no account of itself is not one this page can show.
    let mut v = clean_verdict();
    v["checks"]["drain"] = serde_json::json!({"status": "pass"});
    assert!(canonical()
        .with_verdict(&v)
        .model()
        .evidence
        .verdict
        .is_unavailable());
}

/// Every check of a clean online verdict has to have actually run. A status
/// this page does not recognize — including the empty string — is not a pass.
#[test]
fn a_clean_verdict_whose_checks_did_not_pass_is_inconsistent() {
    for (check, status) in [
        ("drain", ""),
        ("floors", "skipped"),
        ("sink_integrity", "skipped"),
        ("mismatches", "probably-fine"),
        ("canary", "unknown-word"),
    ] {
        let mut v = clean_verdict();
        v["checks"][check]["status"] = serde_json::json!(status);
        let model = canonical().with_verdict(&v).model();
        assert_failure_naming(&model, "a clean run requires it to have passed");
        assert!(
            model
                .evidence
                .verdict_violations
                .iter()
                .any(|s| s.contains(status) || status.is_empty()),
            "{check}={status:?}: {:?}",
            model.evidence.verdict_violations
        );
        assert_ne!(
            model.evidence.sink_state,
            SinkState::VerifiedZero,
            "{check}={status:?} still vouched for an empty sink"
        );
    }

    // The one skip a clean run may legitimately carry.
    let mut v = clean_verdict();
    v["checks"]["canary"] = serde_json::json!({"status": "skipped", "detail": "not requested"});
    assert_eq!(
        canonical().with_verdict(&v).model().banner.state,
        BannerState::Clean
    );
}

/// A config in which nothing is floored makes every reconciliation on the page
/// vacuous — `limen verdict` calls that exit 20, and so does this.
#[test]
fn a_config_that_floors_nothing_is_never_clean() {
    const UNFLOORED: &str = r#"
routes:
  - id: a
    match: { methods: ["GET"], path_prefix: "/a" }
    legacy_upstream: "http://legacy.invalid"
    new_upstream: "http://new.invalid"
    mode: shadow_legacy_primary
    comparison: { enabled: true, sample_rate: 1.0, min_comparisons: 0 }
  - id: b
    match: { methods: ["GET"], path_prefix: "/b" }
    legacy_upstream: "http://legacy.invalid"
    mode: legacy_only
"#;
    let mut v = clean_verdict();
    // No route is floored, so a verdict over it legitimately carries no rows —
    // and would otherwise sail through every check on this page.
    v["floors"] = serde_json::json!([]);
    let model = canonical().with_config(UNFLOORED).with_verdict(&v).model();
    assert_failure_naming(&model, "floors nothing");
    assert!(
        model
            .routes
            .iter()
            .all(|r| r.floor_class != limen::report_html::FloorClass::Met),
        "nothing can be met when nothing is floored"
    );

    // The mirror image, in both directions: a config that *does* floor a route
    // against a verdict carrying no rows for it is caught as drift, and a
    // verdict row for a route the config does not floor is caught as a floor
    // the config never declared.
    let mut v = clean_verdict();
    v["floors"] = serde_json::json!([]);
    assert_failure_naming(
        &canonical().with_verdict(&v).model(),
        "carries no floors row",
    );
    let mut v = clean_verdict();
    v["floors"] = serde_json::json!([
        {"route_id": "b", "comparisons": 3, "floor": 1, "met": true}
    ]);
    let model = canonical().with_verdict(&v).model();
    assert_failure_naming(&model, "carries no floors row");
    assert_failure_naming(&model, "neither compares nor floors it");
}

/// A well-shaped file name over an impossible date is a file limen cannot have
/// written. Counting it would turn "no evidence" into "a file was read and
/// held nothing" — the exact promotion of absence this page exists to refuse.
#[test]
fn an_impossibly_dated_sink_file_is_not_evidence() {
    let ws = canonical();
    std::fs::remove_file(ws.sink_dir.join("mismatches-2026-08-01.jsonl")).unwrap();
    std::fs::write(ws.sink_dir.join("mismatches-2026-99-99.jsonl"), "").unwrap();

    let model = ws.model();
    assert_eq!(
        model.evidence.sink_state,
        SinkState::NoFiles,
        "an impossible date was counted as a sink file"
    );
    assert_ne!(model.banner.state, BannerState::Clean, "{}", why(&model));
    assert!(
        model
            .banner
            .incomplete
            .iter()
            .any(|r| r.contains("no sink files")),
        "{}",
        why(&model)
    );
}

/// An `f64` cannot hold every count a scrape can carry: `2^64` reads back
/// finite, integral and non-negative, then saturates to `u64::MAX` on cast —
/// a count no proxy ever emitted, on an otherwise green page.
#[test]
fn a_counter_too_large_for_an_f64_is_refused_not_saturated() {
    let ws = canonical().with_metrics(
        "limen_comparisons_total{route=\"a\",result=\"match\"} 18446744073709551616\n\
         limen_comparison_skipped_total{route=\"a\",reason=\"event_stream\"} 1\n\
         limen_shadow_requests_total{route=\"a\"} 1\n\
         limen_shadow_failed_total{route=\"a\",reason=\"timeout\"} 0\n",
    );
    let model = ws.model();
    match &model.evidence.metrics {
        Section::Unavailable(reason) => {
            assert!(reason.contains("exact non-negative integer"), "{reason}");
            assert!(reason.contains("18446744073709551616"), "{reason}");
        }
        other => panic!("expected unavailable, got {other:?}"),
    }
    assert_failure_naming(&model, "metrics");
    let html = ws.page();
    assert!(
        !html.contains("18446744073709551615"),
        "the saturated count reached the page"
    );
    assert!(!html.contains("CLEAN"));
}

// ---------------------------------------------------------------------------
// Drift between artifacts
// ---------------------------------------------------------------------------

#[test]
fn a_floors_route_absent_from_the_config_is_drift() {
    let mut v = clean_verdict();
    v["floors"] = serde_json::json!([
        {"route_id": "a", "comparisons": 3, "floor": 1, "met": true},
        {"route_id": "ghost", "comparisons": 3, "floor": 1, "met": true},
    ]);
    let model = canonical().with_verdict(&v).model();
    assert_failure_naming(&model, "ghost");
    assert_failure_naming(&model, "not in this config");
    let ghost = model.routes.iter().find(|r| r.id == "ghost").unwrap();
    assert_eq!(ghost.floor_class, FloorClass::UnknownRoute);
}

#[test]
fn an_unmet_floors_row_for_an_unknown_route_is_still_red() {
    let mut v = clean_verdict();
    v["exit_code"] = serde_json::json!(20);
    v["verdict"] = serde_json::json!("floors-unmet");
    v["checks"]["floors"] = serde_json::json!({"status": "fail", "detail": "ghost"});
    v["floors"] = serde_json::json!([
        {"route_id": "a", "comparisons": 3, "floor": 1, "met": true},
        {"route_id": "ghost", "comparisons": 0, "floor": 1, "met": false},
    ]);
    let model = canonical().with_verdict(&v).model();
    let ghost = model.routes.iter().find(|r| r.id == "ghost").unwrap();
    assert_eq!(
        ghost.floor_class,
        FloorClass::Unmet,
        "an unmet row is rendered on its merits even off the config"
    );
    assert_failure_naming(&model, "floor unmet");
    assert_failure_naming(&model, "not in this config");
}

#[test]
fn sink_counts_disagreeing_with_the_verdict_are_drift() {
    let mut v = clean_verdict();
    v["exit_code"] = serde_json::json!(10);
    v["verdict"] = serde_json::json!("mismatches-found");
    v["mismatches_total"] = serde_json::json!(1);
    v["checks"]["mismatches"] = serde_json::json!({"status": "fail", "detail": "1"});
    v["sink_mismatches_by_route"] = serde_json::json!({"a": 1});
    // The sink on disk holds two, the verdict recorded one.
    let model = canonical()
        .with_verdict(&v)
        .with_sink_lines(&[
            line("2026-08-01T10:00:00Z", "a", "req-1", &["body"]),
            line("2026-08-01T10:00:01Z", "a", "req-2", &["body"]),
        ])
        .model();
    assert_failure_naming(&model, "holds 2 mismatch(es) but the verdict recorded 1");
}

#[test]
fn a_floored_config_route_missing_from_the_verdict_is_drift() {
    let mut v = clean_verdict();
    v["floors"] = serde_json::json!([]);
    v["checks"]["floors"] = serde_json::json!({"status": "pass", "detail": "none"});
    let model = canonical().with_verdict(&v).model();
    assert_failure_naming(&model, "carries no floors row");
    let route_a = model.routes.iter().find(|r| r.id == "a").unwrap();
    assert_eq!(route_a.floor_class, FloorClass::MissingUnexpectedly);
}

#[test]
fn a_floor_the_config_does_not_declare_is_drift() {
    let mut v = clean_verdict();
    v["floors"] = serde_json::json!([
        {"route_id": "a", "comparisons": 3, "floor": 2, "met": true}
    ]);
    let model = canonical().with_verdict(&v).model();
    assert_failure_naming(
        &model,
        "the verdict floored at 2 but this config declares 1",
    );
}

#[test]
fn malformed_sink_lines_fail_the_page() {
    let model = canonical()
        .with_sink_lines(&["{not json".to_string()])
        .model();
    assert_failure_naming(&model, "unparseable line");
    assert_ne!(model.banner.state, BannerState::Clean);
}

#[test]
fn mismatch_records_on_disk_fail_the_page() {
    let ws = canonical().with_sink_lines(&[line(
        "2026-08-01T10:00:00Z",
        "a",
        "req-1",
        &["body", "status"],
    )]);
    let model = ws.model();
    assert_eq!(model.evidence.sink_state, SinkState::Mismatches(1));
    assert_failure_naming(&model, "1 mismatch record(s) on disk");
    let html = ws.page();
    assert!(html.contains("req-1"), "the example is shown");
    assert!(html.contains("MISMATCHES RECORDED"));
}

// ---------------------------------------------------------------------------
// Escaping
// ---------------------------------------------------------------------------

#[test]
fn hostile_route_ids_are_escaped_in_text_and_attribute_positions() {
    let hostile = r#"<script>alert(1)</script>"&'x"#;
    let config = format!(
        r#"
routes:
  - id: {}
    match: {{ methods: ["GET"], path_prefix: "/a" }}
    legacy_upstream: "http://legacy.invalid"
    new_upstream: "http://new.invalid"
    mode: shadow_legacy_primary
    comparison: {{ enabled: true, sample_rate: 1.0 }}
"#,
        serde_json::to_string(hostile).unwrap()
    );
    let ws = Workspace::new()
        .with_config(&config)
        .with_sink_lines(&[line("2026-08-01T10:00:00Z", hostile, "req-1", &["body"])]);
    let model = ws.model();
    assert!(
        model.routes.iter().any(|r| r.id == hostile),
        "the fixture must actually carry the hostile id: {:?}",
        model.routes.iter().map(|r| &r.id).collect::<Vec<_>>()
    );

    let html = ws.page();
    assert!(!html.contains("<script>"), "unescaped tag reached the page");
    assert!(!html.contains("alert(1)</script>"));
    assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(html.contains("&quot;"), "quotes escaped");
    assert!(html.contains("&#39;"), "apostrophes escaped");
    assert!(html.contains("&amp;"), "ampersands escaped");
    // The attribute position is exercised too: the route cell carries a title.
    assert!(html.contains("title=\"route id: &lt;script&gt;"));
    // A stray double quote inside an attribute would end it early.
    for fragment in html.split("title=\"").skip(1) {
        let value = fragment.split('"').next().unwrap();
        assert!(!value.contains('<'), "unescaped markup in an attribute");
    }
}

// ---------------------------------------------------------------------------
// The CLI surface
// ---------------------------------------------------------------------------

fn limen(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_limen"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run limen")
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("exit code")
}

#[test]
fn html_refuses_the_pre_aggregation_filters() {
    let ws = canonical();
    for filter in [["--route", "a"], ["--since", "2026-08-01T00:00:00Z"]] {
        let out = limen(
            ws.dir.path(),
            &[
                "report",
                "--dir",
                ws.sink_dir.to_str().unwrap(),
                "--format",
                "html",
                filter[0],
                filter[1],
            ],
        );
        assert_ne!(code(&out), 0, "{filter:?} was accepted");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(filter[0]), "{stderr}");
    }
}

#[test]
fn an_unwritable_out_is_exit_1() {
    let ws = canonical();
    let out = limen(
        ws.dir.path(),
        &[
            "report",
            "--dir",
            ws.sink_dir.to_str().unwrap(),
            "--format",
            "html",
            "--out",
            "no-such-directory/report.html",
        ],
    );
    assert_eq!(code(&out), 1, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        out.stdout.is_empty(),
        "a failed write emitted a partial page"
    );
}

#[test]
fn dirty_inputs_still_exit_0_with_the_page_on_stdout() {
    let ws = canonical().with_sink_lines(&[
        line("2026-08-01T10:00:00Z", "a", "req-1", &["body"]),
        "{torn".to_string(),
    ]);
    let out = limen(
        ws.dir.path(),
        &[
            "report",
            "--dir",
            ws.sink_dir.to_str().unwrap(),
            "--format",
            "html",
            "--config",
            ws.config.as_ref().unwrap().to_str().unwrap(),
            "--verdict",
            ws.verdict.as_ref().unwrap().to_str().unwrap(),
        ],
    );
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let html = String::from_utf8(out.stdout).unwrap();
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("FAILURE"), "{html}");
    assert!(
        !html.contains("CLEAN"),
        "a dirty run rendered a clean claim"
    );
}

#[test]
fn the_bare_invocation_still_works() {
    // The property the `report` doc comment has always promised: a sink
    // directory alone is enough to run a report anywhere the files are.
    let ws = Workspace::new();
    let out = limen(
        ws.dir.path(),
        &[
            "report",
            "--dir",
            ws.sink_dir.to_str().unwrap(),
            "--format",
            "html",
        ],
    );
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let html = String::from_utf8(out.stdout).unwrap();
    assert!(html.contains("INCOMPLETE"), "{html}");
    assert!(html.contains("NOT PROVIDED"));
}

#[test]
fn verdict_still_has_no_html_format() {
    let ws = canonical();
    let out = limen(
        ws.dir.path(),
        &["verdict", "-c", "limen.config.yaml", "--format", "html"],
    );
    // clap's own usage error, not a verdict exit code.
    assert_eq!(code(&out), 2, "{}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("html"), "{stderr}");
}
