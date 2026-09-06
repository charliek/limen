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
use limen::verdict::CANARY_ROUTE_ID;
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

/// The series limen pre-touches before traffic —
/// `prometheus::register_verdict_series` process-wide and
/// `prometheus::register_skip_series` per route — i.e. what any live limen
/// renders from its very first scrape, and exactly the set
/// `verdict::REQUIRED_SERIES` refuses to read as absent.
///
/// Transcribed from those functions rather than imported from them, so a
/// change to either side shows up here as a failing test rather than as a
/// fixture that silently follows. The per-route half is complete — every
/// reason of every uncompared family for **both** routes [`CONFIG`] declares,
/// including the `legacy_only` one, because that is what `register_skip_series`
/// emits and, given the config, what the page requires series by series rather
/// than family by family.
fn registered() -> String {
    registered_for(&["a", "b"])
}

/// The registered set a limen serving `routes` renders: the process-wide half,
/// then every route's twelve. The route list is the config's, because that is
/// what the registrar walks — a scrape carrying a route the config never
/// declared is drift, and a fixture that carried one would be pinning it.
fn registered_for(routes: &[&str]) -> String {
    let mut out = String::from(
        "limen_shadow_in_flight 0\n\
         limen_diff_sink_enqueued_total 0\n\
         limen_diff_sink_written_total 0\n\
         limen_diff_sink_dropped_total{reason=\"queue_full\"} 0\n\
         limen_diff_sink_dropped_total{reason=\"io_error\"} 0\n\
         limen_diff_sink_dropped_total{reason=\"writer_gone\"} 0\n",
    );
    for route in routes {
        out.push_str(&registered_skips(route));
    }
    out
}

/// The twelve series `register_skip_series` touches for one route: both skip
/// families across every [`limen::observability::metrics::SkipReason`], and the
/// shadow-failure family across both of its reasons. Spelled out rather than
/// generated from those enums, so widening one shows up here.
fn registered_skips(route: &str) -> String {
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
                "{family}{{route=\"{route}\",reason=\"{reason}\"}} 0\n"
            ));
        }
    }
    for reason in ["timeout", "error"] {
        out.push_str(&format!(
            "limen_shadow_failed_total{{route=\"{route}\",reason=\"{reason}\"}} 0\n"
        ));
    }
    out
}

/// Raise one already-registered series off zero, the way traffic would.
/// Substituted rather than appended: a real exporter renders one sample per
/// series, and a fixture that appended a second would be pinning an exposition
/// limen never emits.
fn busy(scrape: &str, series: &str, value: u64) -> String {
    let zero = format!("{series} 0\n");
    assert!(
        scrape.contains(&zero),
        "{series} is not a registered series — the fixture and the registrar disagree"
    );
    scrape.replace(&zero, &format!("{series} {value}\n"))
}

/// A busy proxy's scrape: the registered series with the counters a run that
/// compared and shadowed would have moved, plus the lazily-registered families
/// such a run also touches.
///
/// **The uncompared counters stay at their registered zeros, and that is the
/// fixture's calibration rather than an omission.** This scrape is paired with
/// [`clean_verdict`], whose floors row for `a` claims `skipped: 0,
/// shadow_failures: 0, met: true`. It used to carry three
/// `limen_comparison_skipped_total` skips on that same route — so the positive
/// control this whole file measures "clean" against was two artifacts flatly
/// contradicting each other, rendering CLEAN. A scrape that disagrees with the
/// verdict beside it belongs in a drift test ([`metrics_undermined`]), never in
/// the fixture that defines what agreement looks like.
fn metrics() -> String {
    metrics_for(&["a", "b"])
}

/// The same busy scrape over an arbitrary route set — the traffic all lands on
/// route `a`, which every config in this file declares.
fn metrics_for(routes: &[&str]) -> String {
    format!(
        "{}\
limen_comparisons_total{{route=\"a\",result=\"match\"}} 3
limen_shadow_requests_total{{route=\"a\"}} 3
",
        registered_for(routes)
    )
}

/// The scrape [`undermined_verdict`] was read from: route `a` at its floor,
/// with the two skips and the one shadow failure that row reports, under the
/// very `{metric, reason}` pairs its breakdown names.
fn metrics_undermined() -> String {
    let scrape = busy(
        &metrics(),
        "limen_comparison_skipped_total{route=\"a\",reason=\"response_too_large\"}",
        2,
    );
    busy(
        &scrape,
        "limen_shadow_failed_total{route=\"a\",reason=\"timeout\"}",
        1,
    )
}

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
        "floors": [{
            "route_id": "a",
            "comparisons": 3,
            "floor": 1,
            "floor_met": true,
            "skipped": 0,
            "shadow_failures": 0,
            "uncompared": [],
            "met": true,
        }],
        "sink_mismatches_by_route": {},
        "informational": [],
    })
}

/// The same clean campaign as a limen from before the uncompared gate recorded
/// it: a floors row carrying `met` and the two counts, and none of the fields
/// that split the arithmetic claim from the evidence claim.
fn pre_gating_verdict() -> serde_json::Value {
    let mut v = clean_verdict();
    v["floors"] = serde_json::json!([
        {"route_id": "a", "comparisons": 3, "floor": 1, "met": true}
    ]);
    v
}

/// The verdict `evaluate_floors` writes for an UNDERMINED route: at its floor,
/// and unmet anyway because sampled work on it was never compared.
fn undermined_verdict() -> serde_json::Value {
    let mut v = clean_verdict();
    v["exit_code"] = serde_json::json!(20);
    v["verdict"] = serde_json::json!("floors-unmet");
    v["checks"]["floors"] = serde_json::json!({
        "status": "fail",
        "detail": "route a UNDERMINED: at its floor with 3 uncompared",
    });
    v["floors"][0]["skipped"] = serde_json::json!(2);
    v["floors"][0]["shadow_failures"] = serde_json::json!(1);
    v["floors"][0]["uncompared"] = serde_json::json!([
        {"metric": "limen_comparison_skipped_total", "reason": "response_too_large", "count": 2},
        {"metric": "limen_shadow_failed_total", "reason": "timeout", "count": 1},
    ]);
    v["floors"][0]["met"] = serde_json::json!(false);
    v
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
        .with_metrics(&metrics())
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

    // The other side of that contract, and the reason the incomplete headline
    // does not say "NOT A CLEAN RESULT": a page that is not clean must not
    // contain the word at all, or "did this page claim success?" stops being
    // answerable by searching for it.
    let incomplete = Workspace::new();
    let failure =
        canonical().with_sink_lines(&[line("2026-08-01T10:00:00Z", "a", "req-1", &["body"])]);
    for (expected, ws) in [
        (BannerState::Incomplete, &incomplete),
        (BannerState::Failure, &failure),
    ] {
        let model = ws.model();
        assert_eq!(model.banner.state, expected, "{}", why(&model));
        assert_eq!(
            render(&model).matches("CLEAN").count(),
            0,
            "a {expected:?} page printed the word CLEAN"
        );
    }
    // The skip reasons surface by name — here at their registered zeros, which
    // is what a clean campaign's scrape carries and what the verdict beside it
    // claims. A zero this page can name is the point: it is a count limen
    // recorded, not a family it failed to find.
    assert!(html.contains("event_stream"), "the skip reasons surface");
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
    let ws = Workspace::new()
        .with_config(CONFIG)
        .with_metrics(&metrics());
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
    assert!(ws.page().contains("NOT A PASSING RESULT"));
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

/// A series limen registers at startup, absent: the scrape did not come from a
/// limen control plane, and `limen verdict` would exit 50 on it.
#[test]
fn a_metrics_file_missing_a_required_family_is_unavailable() {
    let without_in_flight: String = registered()
        .lines()
        .filter(|l| !l.starts_with("limen_shadow_in_flight"))
        .map(|l| format!("{l}\n"))
        .collect();
    let ws = canonical().with_metrics(&format!(
        "{without_in_flight}limen_comparisons_total{{route=\"a\",result=\"match\"}} 3\n"
    ));
    let model = ws.model();
    match &model.evidence.metrics {
        Section::Unavailable(why) => {
            assert!(why.contains("limen_shadow_in_flight"), "{why}");
            assert!(why.contains("never a zero count"), "{why}");
        }
        other => panic!("expected unavailable, got {other:?}"),
    }
    assert_eq!(model.banner.state, BannerState::Failure, "{}", why(&model));
}

/// A service that never skipped a comparison is still accepted — but for a
/// different reason than it used to be. Those counters no longer register on
/// their first event: they gate a floored route now, so limen pre-touches them
/// per route at zero, and the page reads a real zero instead of tolerating an
/// absence. `limen_shadow_requests_total` is the family that kept the old
/// tolerance, and a quiet, healthy proxy still depends on it.
#[test]
fn a_scrape_from_a_service_that_never_skipped_is_accepted() {
    let ws = canonical().with_metrics(&format!(
        "{}limen_comparisons_total{{route=\"a\",result=\"match\"}} 3\n",
        registered()
    ));
    let model = ws.model();
    assert!(
        !model.evidence.metrics.is_unavailable(),
        "a quiet service was called a broken one: {:?}",
        model.evidence.metrics
    );
    assert_eq!(model.banner.state, BannerState::Clean, "{}", why(&model));

    // The one absent family is named on the page, not passed over in silence.
    let html = ws.page();
    assert!(html.contains("limen_shadow_requests_total"));
    assert!(html.contains("ABSENT"));
    assert!(html.contains("first event of its kind"));
}

/// The version boundary. A scrape captured from a limen older than the gating
/// change carries none of the three uncompared families, and the page must not
/// read three absent gating counters as three zeros: it refuses the artifact
/// and names what is missing, so the operator knows to rebuild rather than to
/// trust the page.
///
/// Pinned here at the standing this commit gives it — refused, named, and not
/// a clean result. (The FloorDto fields, the Uncompared column, and softening
/// this particular refusal from FAILURE to INCOMPLETE are the status page's own
/// commit; this test exists so that commit cannot silently drop the refusal.)
#[test]
fn a_scrape_from_a_pre_gating_limen_is_refused_by_name() {
    for family in [
        "limen_comparison_skipped_total",
        "limen_shadow_skipped_total",
        "limen_shadow_failed_total",
    ] {
        let pre_gating: String = registered()
            .lines()
            .filter(|l| !l.starts_with(family))
            .map(|l| format!("{l}\n"))
            .collect();
        let ws = canonical().with_metrics(&format!(
            "{pre_gating}limen_comparisons_total{{route=\"a\",result=\"match\"}} 3\n"
        ));
        let model = ws.model();
        match &model.evidence.metrics {
            Section::Unavailable(reason) => assert!(reason.contains(family), "{reason}"),
            other => panic!("expected {family} to be required, got {other:?}"),
        }
        assert_ne!(
            model.banner.state,
            BannerState::Clean,
            "a scrape that cannot show the gating counters is never clean: {}",
            why(&model)
        );
    }
}

/// Even a proxy that has served nothing at all renders: only the four series
/// it registers at startup are required, and `limen verdict` reads an absent
/// `limen_comparisons_total` as zero rather than as a broken scrape.
#[test]
fn a_scrape_from_a_proxy_that_served_nothing_still_renders() {
    let ws = canonical().with_metrics(&registered());
    let model = ws.model();
    assert!(!model.evidence.metrics.is_unavailable());
    assert!(ws.page().contains("reads this as zero"));
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
    // Prefixed with the registered set so the required series are all present
    // and this test can only fail (or pass) on the oversized value itself, not
    // on whichever family `FAMILIES` happens to visit first.
    let ws = canonical().with_metrics(&format!(
        "{}\
         limen_comparisons_total{{route=\"a\",result=\"match\"}} 18446744073709551616\n\
         limen_shadow_requests_total{{route=\"a\"}} 1\n",
        registered()
    ));
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

/// The row this commit exists for. A floored route that reached its floor and
/// still failed it — because a slice of the traffic it was sampled for went
/// uncompared — is a verdict limen really writes. The page must read it as the
/// coherent document it is: no self-contradiction finding, the standing shown
/// as the verdict decided it, and the reason visible on the page rather than
/// only in the JSON.
#[test]
fn an_undermined_floor_renders_without_a_contradiction_finding() {
    let ws = canonical()
        .with_verdict(&undermined_verdict())
        .with_metrics(&metrics_undermined());
    let model = ws.model();
    assert!(
        model.evidence.verdict_violations.is_empty(),
        "a correct verdict was called a torn artifact: {:?}",
        model.evidence.verdict_violations
    );
    assert!(
        !model
            .banner
            .failures
            .iter()
            .any(|f| f.contains("inconsistent verdict artifact")),
        "{}",
        why(&model)
    );
    // Red, and for the right reason: the floor was reached, so "floor unmet"
    // would send the reader to the wrong remedy.
    assert_failure_naming(&model, "floor undermined: route a");
    assert!(
        !model
            .banner
            .failures
            .iter()
            .any(|f| f.contains("floor unmet")),
        "{}",
        why(&model)
    );
    let route_a = model.routes.iter().find(|r| r.id == "a").expect("route a");
    assert_eq!(route_a.floor_class, FloorClass::Unmet);
    let uncompared = route_a.uncompared.as_ref().expect("an uncompared cell");
    assert_eq!(uncompared.total, 3);

    // And on the page: the count, and which reason produced it.
    let html = ws.page();
    assert!(html.contains("Uncompared"), "{html}");
    assert!(html.contains("response_too_large"), "{html}");
    assert!(html.contains("timeout"), "{html}");
    // The cell itself: the total that decided `met`, the reasons that produced
    // it in the cell, and the metric-qualified breakdown in the title.
    let coverage = coverage_row_of(&html, "a");
    assert!(
        coverage.contains("3 <span class=\"pill bad\">response_too_large ×2, timeout ×1</span>"),
        "{coverage}"
    );
    assert!(
        coverage.contains(
            "limen_comparison_skipped_total response_too_large ×2; \
                           limen_shadow_failed_total timeout ×1"
        ),
        "{coverage}"
    );
    // Route b is floored by nothing, so it claims nothing here.
    assert!(
        coverage_row_of(&html, "b").contains("no floors row for this route"),
        "{html}"
    );
}

/// One route's row in the coverage table (section 4).
fn coverage_row_of(html: &str, route: &str) -> String {
    let start = html.find("<h2>4. Coverage").expect("the coverage section");
    let section = &html[start..];
    let needle = format!("title=\"route id: {route}\"");
    let at = section
        .find(&needle)
        .unwrap_or_else(|| panic!("no coverage row for {route}"));
    let row = &section[..at];
    let row_start = row.rfind("<tr>").expect("row start");
    let rest = &section[row_start..];
    let end = rest.find("</tr>").expect("row end");
    rest[..end].to_string()
}

/// Section 7 renders `informational`, which the verdict now fills with
/// unfloored and unknown routes only — a floored route's counters were moved
/// into its floors row, where they gate. Unsaid, that omission reads as "no
/// floored route skipped anything": the same false green the gate exists to
/// kill, relocated into the page's layout.
#[test]
fn the_non_gating_counter_table_says_whose_counters_are_missing_from_it() {
    let mut v = undermined_verdict();
    v["informational"] = serde_json::json!([
        {
            "metric": "limen_shadow_skipped_total",
            "route": "b",
            "reason": "event_stream",
            "value": 7,
        }
    ]);
    // The scrape agrees with both halves of that document: route `a`'s floors
    // row, and route `b`'s informational row.
    let scrape = busy(
        &metrics_undermined(),
        "limen_shadow_skipped_total{route=\"b\",reason=\"event_stream\"}",
        7,
    );
    let html = canonical().with_verdict(&v).with_metrics(&scrape).page();
    let start = html
        .find("Skip and failure counters recorded by the verdict")
        .unwrap();
    let section = &html[start..];
    let end = section.find("<h2>").unwrap_or(section.len());
    let block = &section[..end];
    assert!(block.contains("event_stream"), "{block}");
    assert!(block.contains("Unfloored routes only"), "{block}");
    assert!(block.contains("floors rows"), "{block}");
    assert!(block.contains("Nothing in this table gates."), "{block}");
    // Route a is floored and undermined; its counters belong to section 4.
    assert!(!block.contains("response_too_large"), "{block}");
}

/// The impossible verdict, from the other direction: a row that reached its
/// floor, lost sampled work, and called itself met anyway. Nothing limen writes
/// produces it, so a document carrying it has been edited or torn — and it is
/// exactly the shape that would otherwise ride through as green.
#[test]
fn a_floors_row_claiming_met_over_uncompared_work_is_refused() {
    let mut v = clean_verdict();
    v["floors"][0]["skipped"] = serde_json::json!(3);
    v["floors"][0]["uncompared"] = serde_json::json!([
        {"metric": "limen_shadow_skipped_total", "reason": "concurrency_limit", "count": 3}
    ]);
    let model = canonical().with_verdict(&v).model();
    assert_failure_naming(&model, "inconsistent verdict artifact");
    assert!(
        model
            .banner
            .failures
            .iter()
            .any(|f| f.contains("claims met=true") && f.contains("\"a\"")),
        "the route is named: {}",
        why(&model)
    );
}

/// The other half of that shape: totals of zero beside a per-reason list that
/// says otherwise. The row would render met, and the page green, over work the
/// same row says was never compared.
#[test]
fn a_floors_row_whose_breakdown_contradicts_its_totals_is_refused() {
    let mut v = clean_verdict();
    v["floors"][0]["uncompared"] = serde_json::json!([
        {"metric": "limen_shadow_failed_total", "reason": "error", "count": 5}
    ]);
    let model = canonical().with_verdict(&v).model();
    assert_failure_naming(&model, "do not describe the same run");
}

/// A `verdict.json` written before any of those fields existed still renders.
/// Its zeros are the truth about a run that counted no skips anywhere, so the
/// two new invariants collapse into the old one on it rather than being
/// switched off for it — and the page stays as clean as it was.
#[test]
fn a_verdict_from_before_the_uncompared_gate_still_renders() {
    let ws = canonical().with_verdict(&pre_gating_verdict());
    let model = ws.model();
    assert_eq!(model.banner.state, BannerState::Clean, "{}", why(&model));
    assert!(model.evidence.verdict_violations.is_empty());
    let route_a = model.routes.iter().find(|r| r.id == "a").expect("route a");
    assert_eq!(route_a.floor_class, FloorClass::Met);
    // A floored route that lost nothing reads as a quiet zero, not as noise.
    assert_eq!(route_a.uncompared.as_ref().expect("a cell").total, 0);

    // The old invariant is still enforced on the old shape.
    let mut v = pre_gating_verdict();
    v["floors"][0]["comparisons"] = serde_json::json!(0);
    let model = canonical().with_verdict(&v).model();
    assert_failure_naming(&model, "claims met=true");
}

/// The three uncompared families are registered per configured route, so a
/// config that declares none is owed none. The page used to require them at
/// family level whatever the config said, and called such a scrape's complete
/// instrumentation "absent" — while `limen verdict` correctly called the same
/// run exit 20 for flooring nothing. One input, two tools, and the page's
/// diagnosis was the wrong one.
#[test]
fn a_config_that_declares_no_routes_owes_no_uncompared_series() {
    let ws = canonical()
        .with_config("routes: []\n")
        .with_metrics(&registered_for(&[]));
    let model = ws.model();
    assert!(
        !model.evidence.metrics.is_unavailable(),
        "the scrape was complete for this config: {:?}",
        model.evidence.metrics
    );
    assert!(
        !model
            .banner
            .failures
            .iter()
            .any(|f| f.contains("is absent from the scrape")),
        "{}",
        why(&model)
    );
    // Still never clean — for the reason the verdict gives, not a made-up one.
    assert_failure_naming(&model, "floors nothing");
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

/// **The hole the uncompared gate left open, one artifact up.** The verdict's
/// floors row and the scrape beside it both count the same skips, and until
/// this check nothing compared them — so a verdict saying route `a` lost
/// nothing, next to a scrape saying it lost three comparisons, rendered CLEAN.
/// The page's own doctrine is that a disagreement between two artifacts is a
/// named finding, and this pair was simply one nobody had cross-checked.
///
/// The scrape below is the one this file's canonical fixture used to carry
/// beside [`clean_verdict`], which is how the contradiction survived: the
/// positive control the whole set calibrates "clean" against was itself torn.
#[test]
fn a_floors_row_claiming_no_skips_beside_a_scrape_that_shows_them_is_drift() {
    let scrape = busy(
        &metrics(),
        "limen_comparison_skipped_total{route=\"a\",reason=\"event_stream\"}",
        2,
    );
    let scrape = busy(
        &scrape,
        "limen_comparison_skipped_total{route=\"a\",reason=\"response_buffer_timeout\"}",
        1,
    );
    let ws = canonical().with_metrics(&scrape);
    let model = ws.model();
    // The route, both numbers, and which artifact said which.
    assert_failure_naming(
        &model,
        "route a: the verdict's floors row counted 0 skip(s) against it, but the scrape sums 3",
    );
    // Named on the page too, not only in the model.
    assert!(ws.page().contains("but the scrape sums 3"), "{}", ws.page());
}

/// The same for the other family. A shadow that never answered is uncompared
/// work exactly as a skip is — that is what made it gate — so a verdict that
/// counted none beside a scrape that counted two is the same false green.
#[test]
fn a_floors_row_claiming_no_shadow_failures_beside_a_scrape_that_shows_them_is_drift() {
    let scrape = busy(
        &metrics(),
        "limen_shadow_failed_total{route=\"a\",reason=\"error\"}",
        2,
    );
    let model = canonical().with_metrics(&scrape).model();
    assert_failure_naming(
        &model,
        "route a: the verdict's floors row counted 0 shadow failure(s)",
    );
    assert_failure_naming(&model, "the scrape sums 2");
}

/// The agreeing pair, which must stay silent — the half of this check that
/// keeps a correct campaign from turning red. Both artifacts report the same
/// two skips and the same one shadow failure, and the only failure the page
/// raises is the undermined floor the verdict itself declared.
#[test]
fn a_verdict_and_a_scrape_that_agree_about_uncompared_work_are_not_drift() {
    let model = canonical()
        .with_verdict(&undermined_verdict())
        .with_metrics(&metrics_undermined())
        .model();
    assert!(
        model.evidence.drift.is_empty(),
        "{:?}",
        model.evidence.drift
    );
    assert_failure_naming(&model, "floor undermined: route a");
}

/// **The boundary the equality-vs-monotonic decision turns on.** Counters only
/// rise within one process and the runbook takes the verdict *before* the
/// scrape, so a scrape one skip higher than the verdict could be read as
/// nothing but the clock — and is drift anyway. Uncompared work the gate never
/// saw is uncompared work the gate did not gate on, whichever side of the
/// verdict it landed on; `limen verdict` had already waited for the pipeline to
/// quiesce, so on the documented workflow there was nothing left to move it.
#[test]
fn a_scrape_one_skip_above_the_verdict_is_drift_not_the_clock() {
    // Three skips in the verdict, four in a scrape taken after it.
    let mut v = undermined_verdict();
    v["floors"][0]["skipped"] = serde_json::json!(3);
    v["floors"][0]["uncompared"] = serde_json::json!([
        {"metric": "limen_comparison_skipped_total", "reason": "response_too_large", "count": 3},
        {"metric": "limen_shadow_failed_total", "reason": "timeout", "count": 1},
    ]);
    let scrape = busy(
        &metrics(),
        "limen_comparison_skipped_total{route=\"a\",reason=\"response_too_large\"}",
        4,
    );
    let scrape = busy(
        &scrape,
        "limen_shadow_failed_total{route=\"a\",reason=\"timeout\"}",
        1,
    );
    let model = canonical().with_verdict(&v).with_metrics(&scrape).model();
    assert_failure_naming(
        &model,
        "counted 3 skip(s) against it, but the scrape sums 4",
    );

    // And the mirror image, which no single process can even produce: a verdict
    // claiming a skip the scrape never recorded. Two processes, one page.
    let model = canonical()
        .with_verdict(&v)
        .with_metrics(&metrics_undermined())
        .model();
    assert_failure_naming(
        &model,
        "counted 3 skip(s) against it, but the scrape sums 2",
    );
}

/// A verdict written before the gate never made the claim, so there is nothing
/// to contradict: its missing `skipped`/`shadow_failures` deserialize as zero,
/// and a current scrape beside one may legitimately carry counters it never
/// reported. `floor_met` — absent on exactly those documents and never on a
/// current one — is the signal, the same one the self-consistency invariants
/// use.
#[test]
fn a_pre_gate_verdict_beside_a_scrape_with_uncompared_work_is_not_drift() {
    let scrape = busy(
        &metrics(),
        "limen_comparison_skipped_total{route=\"a\",reason=\"event_stream\"}",
        2,
    );
    let scrape = busy(
        &scrape,
        "limen_shadow_failed_total{route=\"a\",reason=\"timeout\"}",
        1,
    );
    let model = canonical()
        .with_verdict(&pre_gating_verdict())
        .with_metrics(&scrape)
        .model();
    assert!(
        model.evidence.drift.is_empty(),
        "a document that never made the claim was accused of contradicting it: {:?}",
        model.evidence.drift
    );

    // The guard is `floor_met`'s absence and nothing else: the same document
    // with the field restored is held to the current contract.
    let mut v = pre_gating_verdict();
    v["floors"][0]["floor_met"] = serde_json::json!(true);
    let model = canonical().with_verdict(&v).with_metrics(&scrape).model();
    assert_failure_naming(&model, "the verdict's floors row counted 0 skip(s)");
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
// The canary: limen's own record
// ---------------------------------------------------------------------------

/// The verdict as `--canary` renders it: the canary check passed, and the one
/// record it wrote is reported as `canary_records`, outside `mismatches_total`.
fn canary_backed_verdict() -> serde_json::Value {
    let mut v = clean_verdict();
    v["canary_records"] = serde_json::json!(1);
    v["checks"]["canary"] = serde_json::json!({
        "status": "pass",
        "detail": "canary rode compare → sink → flush end-to-end (1 record(s), counters agree)"
    });
    v
}

/// `evaluate_canary` emits `skipped` exactly when zero canary records were
/// counted, so `canary_records: 1` beside a `skipped` canary check is a state
/// no real verdict produces — only a torn or edited artifact carries it, and
/// with the sink's one canary record reconciling (1 == 1) everything else
/// about the page would read clean. It must not.
#[test]
fn a_skipped_canary_with_counted_records_is_an_impossible_verdict() {
    let mut verdict = canary_backed_verdict();
    verdict["checks"]["canary"] =
        serde_json::json!({"status": "skipped", "detail": "--canary not requested"});
    let ws = canonical().with_verdict(&verdict).with_sink_lines(&[line(
        "2026-08-01T10:00:00Z",
        CANARY_ROUTE_ID,
        "canary-1",
        &["body"],
    )]);
    let model = ws.model();
    assert_eq!(model.banner.state, BannerState::Failure, "{}", why(&model));
    assert!(
        model
            .evidence
            .verdict_violations
            .iter()
            .any(|v| v.contains("canary")),
        "{:?}",
        model.evidence.verdict_violations
    );

    // The inverse impossibility: a pass that counted nothing.
    let mut verdict = clean_verdict();
    verdict["checks"]["canary"] = serde_json::json!({"status": "pass", "detail": "impossible"});
    let model = canonical().with_verdict(&verdict).model();
    assert_eq!(model.banner.state, BannerState::Failure, "{}", why(&model));
}

/// **The cold-run regression.** A real campaign with no mismatches leaves no
/// sink file at all — a file is created by the first record written to it — so
/// the only way a clean run has evidence to show is the canary, which rides a
/// record through the real pipeline on purpose. Counting that record as a
/// mismatch turned every canary-backed clean campaign into FAILURE: the page
/// read limen's proof that the sink works as proof that the run was dirty.
#[test]
fn a_canary_backed_clean_campaign_is_clean() {
    let ws = canonical()
        .with_verdict(&canary_backed_verdict())
        .with_sink_lines(&[line(
            "2026-08-01T10:00:00Z",
            CANARY_ROUTE_ID,
            "canary-1",
            &["body"],
        )]);
    let model = ws.model();

    assert_eq!(model.evidence.sink_counts.mismatches, 0);
    assert_eq!(model.evidence.sink_counts.canary, 1);
    assert!(model.evidence.sink_counts.other_reserved.is_empty());
    assert_eq!(
        model.evidence.sink_state,
        SinkState::VerifiedZero,
        "{}",
        why(&model)
    );
    assert_eq!(model.banner.state, BannerState::Clean, "{}", why(&model));
    assert!(
        model.evidence.drift.is_empty(),
        "{:?}",
        model.evidence.drift
    );

    // The canary is neither a route of this campaign nor an unknown one: it is
    // limen's own namespace, and the coverage join has no column for it.
    assert!(
        !model.routes.iter().any(|r| r.id == CANARY_ROUTE_ID),
        "the canary was joined as a route: {:?}",
        model.routes.iter().map(|r| &r.id).collect::<Vec<_>>()
    );

    let html = ws.page();
    assert_eq!(html.matches("CLEAN").count(), 1);
    assert!(html.contains("CANARY RECORDS"), "{html}");
    assert!(html.contains("excluded from the mismatch count"));
    assert!(html.contains("The verdict counted the same 1."));
}

/// The canary is the one record limen writes on purpose, so a disagreement
/// about it is the recording pipeline itself failing — real signal.
#[test]
fn a_canary_count_disagreement_is_a_failure() {
    // The verdict says it wrote one; the sink holds none.
    let model = canonical().with_verdict(&canary_backed_verdict()).model();
    assert_failure_naming(&model, "holds 0 canary record(s) but the verdict counted 1");

    // …and the mirror image: a record on disk no verdict accounts for.
    let ws = canonical().with_sink_lines(&[line(
        "2026-08-01T10:00:00Z",
        CANARY_ROUTE_ID,
        "canary-1",
        &["body"],
    )]);
    let model = ws.model();
    assert_failure_naming(&model, "holds 1 canary record(s) but the verdict counted 0");
    assert!(ws.page().contains("they disagree"));
}

/// Nothing limen writes uses a reserved id other than the canary's, and
/// `verdict`'s per-route reconciliation fails on one for the same reason: no
/// counter can ever match it.
#[test]
fn an_unknown_reserved_route_id_fails() {
    let ws = canonical().with_sink_lines(&[line(
        "2026-08-01T10:00:00Z",
        "__not_a_limen_record__",
        "req-1",
        &["body"],
    )]);
    let model = ws.model();
    assert_eq!(
        model.evidence.sink_counts.mismatches, 0,
        "reserved records are outside the mismatch answer, as verdict has them"
    );
    assert_failure_naming(&model, "__not_a_limen_record__");
    assert!(ws.page().contains("UNKNOWN RESERVED ID"));
}

/// The discipline the empty-directory state points at: without a canary a
/// mismatch-free campaign has no evidence to show, and the page says which
/// flag produces some rather than leaving the operator to guess.
#[test]
fn an_empty_sink_directory_names_the_canary_as_the_way_out() {
    let ws = canonical();
    std::fs::remove_file(ws.sink_dir.join("mismatches-2026-08-01.jsonl")).unwrap();
    let model = ws.model();
    assert_eq!(model.evidence.sink_state, SinkState::NoFiles);
    assert!(
        model
            .banner
            .incomplete
            .iter()
            .any(|r| r.contains("--canary")),
        "{}",
        why(&model)
    );
}

// ---------------------------------------------------------------------------
// Rollout and resilience
//
// The section that has to survive the question a rollout review actually asks:
// what was this route targeting, what did it serve, and was the breaker or a
// stale flag provider quietly answering for it? Every test here tries to make
// the page render a zero it did not read.
// ---------------------------------------------------------------------------

/// The same route table as [`CONFIG`] plus the two rollout modes: a
/// `percentage_split` route with a breaker, and a `failover_to_legacy` one.
const ROLLOUT_CONFIG: &str = r#"
routes:
  - id: a
    match: { methods: ["GET"], path_prefix: "/a" }
    legacy_upstream: "http://legacy.invalid"
    new_upstream: "http://new.invalid"
    mode: shadow_legacy_primary
    comparison: { enabled: true, sample_rate: 1.0 }
  - id: split
    match: { methods: ["GET"], path_prefix: "/split" }
    legacy_upstream: "http://legacy.invalid"
    new_upstream: "http://new.invalid"
    mode: percentage_split
    rollout:
      percentage_flag: "rollout.split.percentage"
      default_percentage: 10
      assignment_key: { header: "x-user-id", fallback: request_random }
    circuit_breaker:
      enabled: true
      failure_rate_threshold: 0.5
      min_requests: 20
      open_duration_ms: 30000
      half_open_max_requests: 5
  - id: failover
    match: { methods: ["GET"], path_prefix: "/failover" }
    legacy_upstream: "http://legacy.invalid"
    new_upstream: "http://new.invalid"
    mode: failover_to_legacy
    failover_safe: true
    circuit_breaker: { enabled: true }
"#;

/// The four transition series a breaker-consulted route registers at startup.
fn transitions(route: &str, counts: [u64; 4]) -> String {
    let [co, oh, hc, ho] = counts;
    format!(
        "limen_breaker_transitions_total{{route=\"{route}\",from=\"closed\",to=\"open\"}} {co}\n\
         limen_breaker_transitions_total{{route=\"{route}\",from=\"open\",to=\"half_open\"}} {oh}\n\
         limen_breaker_transitions_total{{route=\"{route}\",from=\"half_open\",to=\"closed\"}} \
         {hc}\n\
         limen_breaker_transitions_total{{route=\"{route}\",from=\"half_open\",to=\"open\"}} {ho}\n"
    )
}

/// A scrape from a limen serving [`ROLLOUT_CONFIG`]: the registered series, the
/// comparison counters, and every rollout-truth family the control plane
/// refreshes at scrape time.
///
/// The breaker readings are **self-consistent**: a breaker starts closed, so
/// `split`'s two opens, two half-opens and two closes leave it closed again,
/// and `failover`'s untouched breaker is closed with four zero counters. A
/// gauge that disagreed with its own transition history would be an impossible
/// tuple — which the page now rejects, and which a fixture must not smuggle in
/// as the shape of a healthy scrape.
/// Everything [`rollout_metrics`] carries *except* the rollout families: the
/// scrape a limen that predates them would export while serving
/// [`ROLLOUT_CONFIG`] — whose route set is not [`CONFIG`]'s, so the registered
/// half follows it rather than being reused.
fn rollout_base() -> String {
    metrics_for(&["a", "split", "failover"])
}

fn rollout_metrics() -> String {
    format!(
        "{}\
limen_rollout_resolved_target_percentage{{route=\"split\"}} 25
limen_circuit_breaker_state{{route=\"split\",upstream=\"new\"}} 0
limen_circuit_breaker_state{{route=\"failover\",upstream=\"new\"}} 0
{}{}\
limen_flag_provider_stale 0
limen_flag_provider_staleness_seconds 1.5
limen_flag_provider_consecutive_failures 0
limen_requests_total{{route=\"split\",method=\"GET\",upstream=\"new\",status_class=\"2xx\"}} 20
limen_requests_total{{route=\"split\",method=\"GET\",upstream=\"new\",status_class=\"5xx\"}} 10
limen_requests_total{{route=\"split\",method=\"GET\",upstream=\"legacy\",status_class=\"2xx\"}} 70
limen_requests_total{{route=\"failover\",method=\"GET\",upstream=\"new\",status_class=\"2xx\"}} 9
limen_requests_total{{route=\"failover\",method=\"GET\",upstream=\"legacy\",status_class=\"2xx\"}} 1
",
        rollout_base(),
        transitions("split", [2, 2, 2, 0]),
        transitions("failover", [0, 0, 0, 0]),
    )
}

/// The canonical workspace with rollout routes and a scrape that covers them.
fn rollout() -> Workspace {
    canonical()
        .with_config(ROLLOUT_CONFIG)
        .with_metrics(&rollout_metrics())
}

/// Drop every exposition line for `family` from a scrape.
fn without_family(text: &str, family: &str) -> String {
    text.lines()
        .filter(|l| !l.starts_with(family))
        .map(|l| format!("{l}\n"))
        .collect()
}

/// Drop the lines of `family` that carry `route`.
fn without_route_series(text: &str, family: &str, route: &str) -> String {
    text.lines()
        .filter(|l| !(l.starts_with(family) && l.contains(&format!("route=\"{route}\""))))
        .map(|l| format!("{l}\n"))
        .collect()
}

/// A scrape that is a limen scrape in every respect but the rollout families.
fn rollout_metrics_without(family: &str) -> String {
    without_family(&rollout_metrics(), family)
}

/// Just the rollout section's HTML. Assertions about what this section does —
/// or does not — say must not be satisfied, or defeated, by another one.
fn rollout_section(html: &str) -> String {
    let start = html.find("<h2>5. Rollout").expect("the rollout section");
    let rest = &html[start..];
    let end = rest.find("<h2>6.").unwrap_or(rest.len());
    rest[..end].to_string()
}

/// One route's row in the rollout truth table — the first row for that id in
/// the section, which is the truth table's (the "as configured" table renders
/// after it).
fn rollout_row_of(html: &str, route: &str) -> String {
    let needle = format!("title=\"route id: {route}\"");
    rollout_section(html)
        .lines()
        .find(|l| l.starts_with("<tr>") && l.contains(&needle))
        .unwrap_or_else(|| panic!("no rollout row for {route}"))
        .to_string()
}

/// A row that carries no truth at all: one spanning unavailable cell, and none
/// of the five reading columns.
fn assert_row_is_unavailable(html: &str, route: &str) {
    let row = rollout_row_of(html, route);
    assert!(
        row.contains("colspan=\"5\"")
            && row.contains("<span class=\"pill bad\">UNAVAILABLE</span>"),
        "row for {route} is not the spanning unavailable row: {row}"
    );
    // Nothing that could be read as a reading. Scoped to what precedes the
    // spanning cell: the rejection sentence inside it necessarily quotes the
    // series, states and values it is refusing, and that prose is the point —
    // it is the *cells* that must carry no truth.
    let cells = row.split("colspan=\"5\"").next().expect("the row prefix");
    for fabricated in [
        "%",
        "closed",
        "half-open",
        "open",
        "→",
        "pill good",
        "pill warn",
    ] {
        assert!(
            !cells.contains(fabricated),
            "row for {route} fabricated {fabricated:?} in a cell: {row}"
        );
    }
}

#[test]
fn the_rollout_section_reports_target_share_breaker_and_transitions() {
    let ws = rollout();
    let model = ws.model();
    assert_eq!(model.banner.state, BannerState::Clean, "{}", why(&model));
    let html = ws.page();
    let section = rollout_section(&html);

    assert!(html.contains("Rollout &amp; resilience"), "{html}");
    // The target the rollout asked for, from the gauge — not a config default.
    assert!(
        section.contains("25%"),
        "the resolved target is on the page"
    );
    assert!(
        !rollout_row_of(&html, "split").contains("10%"),
        "the config default must not be rendered as the resolved target"
    );
    // The share actually served, with the counts it was computed from — each
    // side labelled, so an absent one can never look like a counted zero.
    assert!(section.contains("30% (new: 30 / legacy: 70)"), "{section}");
    assert!(section.contains("90% (new: 9 / legacy: 1)"), "{section}");
    // Breaker state by name, never by gauge number.
    assert!(section.contains(">closed<"), "{section}");
    // The four transitions, named as pairs — including the zero: a breaker
    // that never opened is the answer, and an omitted count is a guess.
    assert!(
        section.contains("closed→open 2, open→half-open 2, half-open→closed 2, half-open→open 0"),
        "{section}"
    );
    // The flag provider's own standing, above the table.
    assert!(section.contains("FRESH"), "{section}");
    assert!(section.contains("1.5"), "the staleness reading is shown");
    // The config side of each row.
    assert!(section.contains("rollout.split.percentage"), "{section}");
    assert!(section.contains("x-user-id"), "{section}");
    assert!(section.contains("failover_to_legacy"), "{section}");
}

#[test]
fn a_split_route_missing_its_target_series_is_unavailable_never_zero() {
    let ws = rollout().with_metrics(&without_route_series(
        &rollout_metrics(),
        "limen_rollout_resolved_target_percentage",
        "split",
    ));
    let model = ws.model();
    assert_failure_naming(&model, "split");
    assert!(
        model
            .banner
            .failures
            .iter()
            .any(|f| f.contains("limen_rollout_resolved_target_percentage")),
        "{}",
        why(&model)
    );
    let html = ws.page();
    // The other route's row survives intact — one lost series is not the whole
    // section's problem.
    assert!(rollout_row_of(&html, "failover").contains("90% (new: 9 / legacy: 1)"));
    // …and the lost one renders no target at all. Not `0%`, and not the
    // config's `default_percentage` standing in for the reading.
    assert_row_is_unavailable(&html, "split");
}

#[test]
fn a_duplicated_target_series_rejects_the_row_rather_than_picking_one() {
    let ws = rollout().with_metrics(&format!(
        "{}limen_rollout_resolved_target_percentage{{route=\"split\"}} 75\n",
        rollout_metrics()
    ));
    let model = ws.model();
    assert_failure_naming(&model, "split");
    assert!(
        model
            .banner
            .failures
            .iter()
            .any(|f| f.contains("more than one")),
        "{}",
        why(&model)
    );
    let html = ws.page();
    assert_row_is_unavailable(&html, "split");
    for either in ["25%", "75%"] {
        assert!(
            !rollout_section(&html).contains(either),
            "a rejected row still rendered a target: {either}"
        );
    }
}

/// A partial transition set is not a breaker that made three of four kinds of
/// move: all four are registered at startup. The row renders as one spanning
/// unavailable rather than three real counts and a fabricated zero.
#[test]
fn a_missing_transition_series_rejects_the_row() {
    let ws = rollout().with_metrics(&without_family(
        &rollout_metrics(),
        "limen_breaker_transitions_total{route=\"split\",from=\"half_open\",to=\"open\"}",
    ));
    let model = ws.model();
    assert_failure_naming(&model, "limen_breaker_transitions_total");
    assert!(
        model.banner.failures.iter().any(|f| f.contains("split")),
        "{}",
        why(&model)
    );
    let html = ws.page();
    assert_row_is_unavailable(&html, "split");
    assert!(
        !rollout_row_of(&html, "split").contains("closed→open"),
        "three surviving counts were rendered as the whole history"
    );
    // The route that kept all four is untouched.
    assert!(rollout_row_of(&html, "failover").contains("half-open→open 0"));
}

/// The page's whole reason for existing, in one cell: a stale flag provider
/// puts every split route at 0%, and a bare "0%" there reads as a rollout
/// somebody turned down rather than one that was displaced.
#[test]
fn stale_flags_never_render_as_a_clean_zero() {
    let stale = rollout_metrics()
        .replace(
            "limen_rollout_resolved_target_percentage{route=\"split\"} 25",
            "limen_rollout_resolved_target_percentage{route=\"split\"} 0",
        )
        .replace("limen_flag_provider_stale 0", "limen_flag_provider_stale 1")
        .replace(
            "limen_flag_provider_staleness_seconds 1.5",
            "limen_flag_provider_staleness_seconds 900",
        )
        .replace(
            "limen_flag_provider_consecutive_failures 0",
            "limen_flag_provider_consecutive_failures 12",
        );
    let ws = rollout().with_metrics(&stale);
    let model = ws.model();
    assert_failure_naming(&model, "stale");
    let html = ws.page();
    assert!(html.contains("STALE"), "{html}");
    assert!(html.contains("fail-safe"), "{html}");
    assert!(html.contains("12 consecutive"), "{html}");
    // The joined truth, in the cell itself: never the number on its own.
    assert!(html.contains("0% — fail-safe (flags stale)"), "{html}");
    assert!(
        !html.contains("<td>0%</td>"),
        "a bare fail-safe zero reached the page"
    );
}

/// A stale provider displaces the rollout outright (`fail_safe_mode:
/// legacy_only`), so a nonzero target beside `stale 1` is a state no limen
/// produces.
#[test]
fn a_nonzero_target_under_stale_flags_is_a_contradiction() {
    // A *coherent* stale provider — stale past the 30s TTL — so the finding
    // under test is the target beside it, not the provider tuple.
    let ws = rollout().with_metrics(
        &rollout_metrics()
            .replace("limen_flag_provider_stale 0", "limen_flag_provider_stale 1")
            .replace(
                "limen_flag_provider_staleness_seconds 1.5",
                "limen_flag_provider_staleness_seconds 120",
            ),
    );
    let model = ws.model();
    assert_failure_naming(&model, "split");
    assert!(
        model
            .banner
            .failures
            .iter()
            .any(|f| f.contains("stale") && f.contains("25")),
        "{}",
        why(&model)
    );
}

/// A scrape from a limen that predates the rollout gauges carries no rollout
/// truth at all. Rendering that as an empty table would say the rollout was
/// fine; the page says the scrape cannot answer.
#[test]
fn a_scrape_with_no_rollout_families_at_all_is_unavailable() {
    // `rollout_base()` is the pre-rollout scrape: the series this config's
    // routes register, plus comparison counters, and nothing else.
    let ws = rollout().with_metrics(&rollout_base());
    let model = ws.model();
    assert_failure_naming(&model, "no rollout truth");
    assert!(model.evidence.rollout.is_unavailable());
    let html = ws.page();
    assert!(html.contains("Rollout &amp; resilience"), "{html}");
    assert!(rollout_section(&html).contains("UNAVAILABLE"), "{html}");
}

/// The honest empty: a config with neither rollout mode has no rollout to
/// report, and that is one line rather than a red section.
#[test]
fn a_config_without_rollout_routes_says_so_in_one_line() {
    let ws = canonical();
    let model = ws.model();
    assert_eq!(model.banner.state, BannerState::Clean, "{}", why(&model));
    let html = ws.page();
    assert!(html.contains("Rollout &amp; resilience"), "{html}");
    assert!(
        html.contains("declares no percentage_split or failover_to_legacy route"),
        "{html}"
    );
}

/// The existing contract for a metrics-dependent section, mirrored: no scrape
/// is an absence of evidence, not a set of zeros.
#[test]
fn without_a_metrics_scrape_the_rollout_section_is_not_provided() {
    let ws = Workspace::new()
        .with_config(ROLLOUT_CONFIG)
        .with_verdict(&clean_verdict());
    let model = ws.model();
    // Nothing about the rollout may turn a missing optional input into red.
    assert!(
        model.banner.failures.is_empty(),
        "a missing scrape is not a rollout failure: {}",
        why(&model)
    );
    // The variant itself, not just a word on the page: NOT PROVIDED and
    // UNAVAILABLE are different claims and this is the one that must hold.
    assert!(
        matches!(model.evidence.rollout, Section::NotProvided),
        "{:?}",
        model.evidence.rollout
    );
    let html = ws.page();
    let section = rollout_section(&html);
    assert!(html.contains("Rollout &amp; resilience"), "{html}");
    assert!(section.contains("NOT PROVIDED"), "{section}");
    assert!(!section.contains("UNAVAILABLE"), "{section}");
    // The config's rollout settings are not shown either: a table of what the
    // rollout was *asked* to do reads as a report on what it did.
    assert!(!section.contains("rollout.split.percentage"), "{section}");
}

#[test]
fn impossible_rollout_values_are_refused_row_by_row() {
    let cases: [(&str, &str, &str); 3] = [
        (
            "limen_rollout_resolved_target_percentage{route=\"split\"} 25",
            "limen_rollout_resolved_target_percentage{route=\"split\"} 250",
            "250",
        ),
        (
            "limen_circuit_breaker_state{route=\"split\",upstream=\"new\"} 0",
            "limen_circuit_breaker_state{route=\"split\",upstream=\"new\"} 7",
            "7",
        ),
        (
            "limen_breaker_transitions_total{route=\"split\",from=\"closed\",to=\"open\"} 2",
            "limen_breaker_transitions_total{route=\"split\",from=\"closed\",to=\"open\"} 1.5",
            "1.5",
        ),
    ];
    for (from, to, quoted) in cases {
        let ws = rollout().with_metrics(&rollout_metrics().replace(from, to));
        let model = ws.model();
        assert_failure_naming(&model, "split");
        assert!(
            model.banner.failures.iter().any(|f| f.contains(quoted)),
            "no failure quotes {quoted:?}: {}",
            why(&model)
        );
        let html = ws.page();
        // The invalid row renders no truth cells at all — not the readings
        // that happened to parse beside the one that did not. (The refused
        // value appears only inside the rejection sentence, which quotes it.)
        assert_row_is_unavailable(&html, "split");
        let row = rollout_row_of(&html, "split");
        let cells = row.split("colspan=\"5\"").next().expect("the row prefix");
        assert!(
            !cells.contains(quoted),
            "the refused value {quoted:?} was rendered as a reading: {row}"
        );
        // The failover row is untouched by the split row's bad value.
        assert!(rollout_row_of(&html, "failover").contains("90% (new: 9 / legacy: 1)"));
    }
}

/// `limen_requests_total` registers on the first request of its kind, so a
/// route serving 0% to new legitimately carries no `new` series — the same
/// shape a lost counter takes. Absence stays non-failing (the scrape-level
/// question is already settled by the required families), but it may never
/// render as a counted zero: the annotation is what keeps the two apart.
#[test]
fn an_absent_request_counter_is_annotated_never_a_bare_share() {
    // One side absent: the share is still stated, and says which side was
    // never counted rather than presenting a bare percentage.
    let one_side = without_family(
        &rollout_metrics(),
        "limen_requests_total{route=\"split\",method=\"GET\",upstream=\"new\"",
    );
    let ws = rollout().with_metrics(&one_side);
    let model = ws.model();
    assert_eq!(model.banner.state, BannerState::Clean, "{}", why(&model));
    let row = rollout_row_of(&ws.page(), "split");
    assert!(
        row.contains("0% (new: no series = zero recorded / legacy: 70)"),
        "a half-known pair rendered without its annotation: {row}"
    );
    assert!(
        !row.contains("NO SHARE"),
        "a half-known pair took the no-traffic label: {row}"
    );

    // Both sides absent: no share at all, and the informational label says the
    // series are missing rather than that the split served nothing to new.
    let ws = rollout().with_metrics(&without_route_series(
        &rollout_metrics(),
        "limen_requests_total",
        "split",
    ));
    let model = ws.model();
    assert_eq!(model.banner.state, BannerState::Clean, "{}", why(&model));
    let html = ws.page();
    assert!(
        rollout_section(&html).contains("25%"),
        "the target is known"
    );
    let row = rollout_row_of(&html, "split");
    assert!(row.contains("NO SHARE"), "{row}");
    assert!(
        row.contains("no series = zero recorded"),
        "the absent sides are named: {row}"
    );
}

/// The flag gauges are refreshed on every scrape of a limen control plane, so
/// their absence beside live rollout series is a torn or foreign scrape.
#[test]
fn absent_flag_gauges_beside_live_rollout_series_are_a_failure() {
    let ws = rollout().with_metrics(&rollout_metrics_without("limen_flag_provider_stale"));
    let model = ws.model();
    assert_failure_naming(&model, "limen_flag_provider_stale");
}

/// A diverting breaker is not a clean campaign: an open one means the new
/// upstream was never exercised, a half-open one that it was on probation.
/// Either way the coverage is not what the config asked for.
#[test]
fn a_diverting_breaker_is_never_a_clean_page() {
    // Self-consistent tuples: `[1, 0, 0, 0]` leaves the breaker open (one entry
    // into open, no exit); `[1, 1, 0, 0]` leaves it half-open.
    for (state, counts, word) in [(2, [1, 0, 0, 0], "open"), (1, [1, 1, 0, 0], "half-open")] {
        let text = rollout_metrics()
            .replace(
                "limen_circuit_breaker_state{route=\"split\",upstream=\"new\"} 0",
                &format!("limen_circuit_breaker_state{{route=\"split\",upstream=\"new\"}} {state}"),
            )
            .replace(
                &transitions("split", [2, 2, 2, 0]),
                &transitions("split", counts),
            );
        let ws = rollout().with_metrics(&text);
        let model = ws.model();
        assert_failure_naming(&model, &format!("breaker {word} on route split"));
        // The row is still readable — a diverting breaker is a finding, not an
        // unreadable row.
        assert!(rollout_row_of(&ws.page(), "split").contains(&format!(">{word}<")));
    }
}

/// C-L1 writes this gauge for every route with a breaker on every scrape, so
/// its absence is a scrape that cannot say whether the breaker was diverting —
/// which is the one thing that must never default to "closed".
#[test]
fn an_absent_breaker_state_rejects_the_row() {
    let ws = rollout().with_metrics(&without_route_series(
        &rollout_metrics(),
        "limen_circuit_breaker_state",
        "split",
    ));
    let model = ws.model();
    assert_failure_naming(&model, "limen_circuit_breaker_state");
    // `assert_row_is_unavailable` already refuses any state word in a cell;
    // this pins the specific default the absence must never become.
    assert_row_is_unavailable(&ws.page(), "split");
}

/// The breaker guards the new upstream and nothing else, so a state series
/// under another label — or none — is not this breaker's state.
#[test]
fn a_breaker_state_under_another_upstream_is_malformed() {
    for label in ["upstream=\"legacy\"", "upstream=\"\""] {
        let ws = rollout().with_metrics(&rollout_metrics().replace(
            "limen_circuit_breaker_state{route=\"split\",upstream=\"new\"}",
            &format!("limen_circuit_breaker_state{{route=\"split\",{label}}}"),
        ));
        let model = ws.model();
        assert_failure_naming(&model, "the breaker guards the new upstream");
        assert_row_is_unavailable(&ws.page(), "split");
    }
}

/// The gauge and the transition counts are two readings of one breaker taken
/// microseconds apart, so a one-step difference is the race between them — and
/// says so, reporting the more diverting of the two rather than picking one.
#[test]
fn a_one_step_state_skew_is_reported_not_rejected() {
    // Counts `[3, 2, 2, 0]` leave the breaker open; the gauge still reads the
    // closed it was refreshed at, one transition earlier.
    let ws = rollout().with_metrics(&rollout_metrics().replace(
        &transitions("split", [2, 2, 2, 0]),
        &transitions("split", [3, 2, 2, 0]),
    ));
    let model = ws.model();
    let row = rollout_row_of(&ws.page(), "split");
    assert!(row.contains("STATE/COUNTERS SKEWED"), "{row}");
    assert!(
        row.contains(">open<"),
        "the skew reported the calmer of the two readings: {row}"
    );
    // Reported as open, so the diverting-breaker failure applies — a skew is
    // never a way to a clean page.
    assert_failure_naming(&model, "breaker open on route split");
}

/// A tuple no history can produce is not a race, it is a scrape that was
/// edited, merged, or is not limen's.
#[test]
fn an_impossible_transition_tuple_rejects_the_row() {
    // Two closes against one open: the breaker left the closed state once and
    // returned to it twice.
    let ws = rollout().with_metrics(&rollout_metrics().replace(
        &transitions("split", [2, 2, 2, 0]),
        &transitions("split", [1, 2, 2, 0]),
    ));
    let model = ws.model();
    assert_failure_naming(&model, "describe no history a breaker can have");
    assert_row_is_unavailable(&ws.page(), "split");
}

/// `Closed` and `HalfOpen` are not connected by any single legal transition —
/// the state machine only ever reaches `HalfOpen` from `Open` — so a scrape
/// pairing them is rejected outright. This is not a question of which
/// reading is "ahead": counts `[3, 3, 2, 0]` are, if anything, numerically
/// ahead of the `[2, 2, 2, 0]` baseline in every column that moved, and it is
/// still rejected, because no edge in [`BreakerState::TRANSITIONS`] runs
/// `closed`→`half_open`.
#[test]
fn a_gauge_and_counters_pair_with_no_legal_edge_rejects_the_row() {
    // Counts leave the breaker half-open; the gauge says closed — and no
    // legal transition connects `closed` to `half_open` directly.
    let ws = rollout().with_metrics(&rollout_metrics().replace(
        &transitions("split", [2, 2, 2, 0]),
        &transitions("split", [3, 3, 2, 0]),
    ));
    let model = ws.model();
    assert_failure_naming(&model, "not describing the same breaker");
    assert_row_is_unavailable(&ws.page(), "split");
}

/// The other legitimate cause of a one-step skew, running the opposite
/// direction from [`a_one_step_state_skew_is_reported_not_rejected`]:
/// `CircuitBreaker::state()` reports `HalfOpen` the instant the open window
/// elapses, without touching the counter — the counter only bumps inside the
/// next admitted request. A scrape landing in that gap sees a gauge already
/// at `half_open` while the transition counts still describe the breaker as
/// `open`. It is accepted through the same `half_open`→`open` edge that also
/// covers a failed half-open trial reopening the breaker — the two races are
/// indistinguishable from one scrape, and both are legal.
#[test]
fn a_gauge_ahead_of_a_lazy_half_open_read_is_reported_not_rejected() {
    // Counts `[3, 2, 2, 0]` leave the breaker open by the transition history;
    // the gauge already reads half-open, having advanced ahead of it.
    let ws = rollout().with_metrics(
        &rollout_metrics()
            .replace(
                "limen_circuit_breaker_state{route=\"split\",upstream=\"new\"} 0",
                "limen_circuit_breaker_state{route=\"split\",upstream=\"new\"} 1",
            )
            .replace(
                &transitions("split", [2, 2, 2, 0]),
                &transitions("split", [3, 2, 2, 0]),
            ),
    );
    let model = ws.model();
    let row = rollout_row_of(&ws.page(), "split");
    assert!(row.contains("STATE/COUNTERS SKEWED"), "{row}");
    assert!(
        row.contains(">open<"),
        "the skew reported the more diverting of the two readings: {row}"
    );
    // Reported as open, so the diverting-breaker failure applies — a skew is
    // never a way to a clean page.
    assert_failure_naming(&model, "breaker open on route split");
}

/// The three flag gauges come from one `health()` snapshot, so a tuple that
/// disagrees with itself is corruption — not a provider in an odd state.
#[test]
fn an_incoherent_flag_tuple_is_rejected() {
    let swap = |from: &str, to: &str| rollout_metrics().replace(from, to);
    let cases = [
        // Fresh, but no successful refresh has ever happened.
        (
            swap(
                "limen_flag_provider_staleness_seconds 1.5",
                "limen_flag_provider_staleness_seconds -1",
            ),
            "never refreshed",
        ),
        // Fresh, but older than this config's 30s stale_ttl_ms.
        (
            swap(
                "limen_flag_provider_staleness_seconds 1.5",
                "limen_flag_provider_staleness_seconds 45",
            ),
            "past this config's stale_ttl_ms",
        ),
        // Stale, but well inside the TTL.
        (
            swap("limen_flag_provider_stale 0", "limen_flag_provider_stale 1"),
            "inside this config's stale_ttl_ms",
        ),
    ];
    for (text, needle) in cases {
        let ws = rollout().with_metrics(&text);
        let model = ws.model();
        assert_failure_naming(&model, needle);
        assert!(
            rollout_section(&ws.page()).contains("FLAG PROVIDER INCOHERENT"),
            "the provider block still rendered as a reading"
        );
    }

    // …and the legal tuples stay legal, including both sides of the boundary.
    for (stale, age) in [(0, "30"), (1, "30"), (1, "-1"), (0, "0")] {
        let text = rollout_metrics()
            .replace(
                "limen_flag_provider_stale 0",
                &format!("limen_flag_provider_stale {stale}"),
            )
            .replace(
                "limen_flag_provider_staleness_seconds 1.5",
                &format!("limen_flag_provider_staleness_seconds {age}"),
            );
        let model = rollout().with_metrics(&text).model();
        assert!(
            !model
                .banner
                .failures
                .iter()
                .any(|f| f.contains("did not come from one health snapshot")
                    || f.contains("stale_ttl_ms")),
            "stale={stale} age={age} was called incoherent: {}",
            why(&model)
        );
    }
}

/// `register_rollout_series` emits exactly the set the config implies, so a
/// series for a route the config never declared — or one it gives no way to
/// produce — means the scrape and the config are different deployments.
#[test]
fn rollout_series_the_config_cannot_account_for_are_a_failure() {
    // A route the config has never heard of.
    let ws = rollout().with_metrics(&format!(
        "{}limen_rollout_resolved_target_percentage{{route=\"ghost\"}} 5\n",
        rollout_metrics()
    ));
    let model = ws.model();
    assert_failure_naming(&model, "route ghost");
    assert_failure_naming(&model, "different route tables");

    // A configured route that cannot own the series: `a` is shadow-only, so it
    // resolves no rollout target.
    let ws = rollout().with_metrics(&format!(
        "{}limen_rollout_resolved_target_percentage{{route=\"a\"}} 5\n",
        rollout_metrics()
    ));
    let model = ws.model();
    assert_failure_naming(&model, "no way to produce one");
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
