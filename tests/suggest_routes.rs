//! `limen suggest-routes` through the real binary: the exit-code vocabulary and
//! the property the whole command rests on — **the emitted draft is a
//! configuration limen accepts**, proven by running `limen validate-config`
//! over it rather than by parsing it in-process.
//!
//! The input config carries every route shape that has its own validation rule:
//! `shadow_methods` with a verdict floor, a contract reference, a catch-all, a
//! `percentage_split` with its required `rollout`, a `failover_to_legacy` with
//! `failover_safe`, a provably-disjoint pair of query-conditioned routes, and a
//! `new_only` route with no legacy upstream. "The draft always loads" is only
//! worth asserting against the modes that are hard to emit.
//!
//! Profile fixtures are **built from the real `ObserveProfile` model and
//! serialized**, never hand-written JSON: the document is strict (every field
//! required), so a hand-written fixture would rot into an unparseable one and a
//! partial one must fail rather than zero-fill.
//!
//! Driven through `--profile` files, so no proxy is bound and no test here
//! touches the process-global metrics recorder. The live control-plane path
//! (quiescence, exit 40) lives in its own binaries for exactly that reason.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Output};

use limen::observability::observe::{ObserveProfile, RouteProfile};

/// Routes carrying every shape a wholesale-replacement bug — or a mode rewrite
/// — would trip over.
const HOSTILE_CONFIG: &str = r#"
observe: {}
flags:
  provider: file
  file: { path: "./flags.local.yaml" }
routes:
  - id: pat-validate
    match: { methods: ["GET", "POST"], path_prefix: "/api/v1/pat/validate" }
    legacy_upstream: "http://legacy.internal"
    new_upstream: "http://new.internal"
    mode: shadow_legacy_primary
    comparison:
      enabled: true
      sample_rate: 1.0
      max_body_bytes: 1048576
      min_comparisons: 20
      shadow_methods: ["POST"]
  - id: contracted
    match: { methods: ["GET"], path_prefix: "/devices" }
    legacy_upstream: "http://legacy.internal"
    new_upstream: "http://new.internal"
    mode: shadow_legacy_primary
    contract: "svc.contract.yaml#get-device"
  - id: split
    match: { methods: ["GET"], path_prefix: "/split" }
    legacy_upstream: "http://legacy.internal"
    new_upstream: "http://new.internal"
    mode: percentage_split
    rollout:
      percentage_flag: "split.percentage"
      default_percentage: 10
      assignment_key: { header: "x-user-id", fallback: request_random }
  - id: failover
    match: { methods: ["GET", "POST"], path_prefix: "/failover" }
    legacy_upstream: "http://legacy.internal"
    new_upstream: "http://new.internal"
    mode: failover_to_legacy
    failover_safe: true
  - id: verifier-hop
    match: { methods: ["GET"], path_prefix: "/oauth2/auth", query_present: ["login_verifier"] }
    legacy_upstream: "http://legacy.internal"
    mode: legacy_only
  - id: oauth2-auth
    match: { methods: ["GET"], path_prefix: "/oauth2/auth", query_absent: ["login_verifier"] }
    legacy_upstream: "http://legacy.internal"
    mode: legacy_only
  - id: migrated
    match: { methods: ["GET"], path_prefix: "/migrated" }
    new_upstream: "http://new.internal"
    mode: new_only
  - id: catchall
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "http://legacy.internal"
    mode: legacy_only
"#;

/// The contract `contracted` references, so the input (and any draft that keeps
/// the reference) resolves.
const CONTRACT: &str = r#"
version: 1
service: svc
routes:
  - id: "get-device"
    match: { methods: ["GET"], path_template: "/devices/{id}" }
"#;

/// A route that looks like a clean, stable read: one path, one content type,
/// repeats at a stable length. Candidate-shaped unless a config-derived rule
/// (R1's catch-all, R6's `query_present`) says otherwise.
///
/// `basis` is the matcher the route was profiled under (`prefix:/devices`,
/// `template:/conversations/{id}`) and must agree with the config the fixture
/// is classified against — a profile that disagrees is a *stale* profile, which
/// the command refuses, and refusing it is the subject of its own tests below.
fn stable_reads(reads: u64, basis: &str) -> RouteProfile {
    RouteProfile {
        match_basis: basis.to_string(),
        observations: reads,
        reads,
        distinct_read_paths: 1,
        status_classes: BTreeMap::from([("2xx".to_string(), reads)]),
        content_types: BTreeSet::from(["application/json".to_string()]),
        length_repeats: reads / 2,
        ..RouteProfile::default()
    }
}

fn profile_document(sample_rate: f64, routes: Vec<(&str, RouteProfile)>) -> String {
    let profile = ObserveProfile {
        sample_rate,
        routes: routes
            .into_iter()
            .map(|(id, p)| (id.to_string(), p))
            .collect(),
    };
    serde_json::to_string(&profile).expect("serialize profile")
}

/// The routes the harness classifies. Every route is given candidate-shaped
/// traffic, so a route that ends up uncompared did so for a *reason* rather
/// than for lack of evidence — `contracted` excepted, which is missing some
/// `Content-Length`s (R11) and is therefore the narrowed route.
///
/// Returned unserialized so a test that needs one route bent out of shape can
/// bend that one and keep the rest: the list must agree with `HOSTILE_CONFIG`,
/// and a second copy of it is a copy that goes stale in silence.
fn hostile_routes() -> Vec<(&'static str, RouteProfile)> {
    let mut contracted = stable_reads(20, "prefix:/devices");
    contracted.length_missing = 3;
    contracted.distinct_read_paths = 2;
    let mut pat_validate = stable_reads(34, "prefix:/api/v1/pat/validate");
    pat_validate.observations = 40;
    pat_validate.writes = 6;
    vec![
        ("pat-validate", pat_validate),
        ("contracted", contracted),
        ("split", stable_reads(20, "prefix:/split")),
        ("failover", stable_reads(20, "prefix:/failover")),
        ("verifier-hop", stable_reads(9, "prefix:/oauth2/auth")),
        ("oauth2-auth", stable_reads(16, "prefix:/oauth2/auth")),
        ("migrated", stable_reads(12, "prefix:/migrated")),
        ("catchall", stable_reads(9, "prefix:/")),
    ]
}

/// [`hostile_routes`] as the document the command reads, at a full sample.
fn hostile_profile() -> String {
    profile_document(1.0, hostile_routes())
}

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

/// Lay out a working directory holding the config, the contract and the
/// profile. Returns the tempdir (kept alive by the caller).
fn workspace(config: &str, profile: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("limen.config.yaml"), config).expect("config");
    std::fs::write(dir.path().join("svc.contract.yaml"), CONTRACT).expect("contract");
    std::fs::write(dir.path().join("profile.json"), profile).expect("profile");
    dir
}

fn suggest(dir: &Path, extra: &[&str]) -> Output {
    let mut args = vec![
        "suggest-routes",
        "-c",
        "limen.config.yaml",
        "--profile",
        "profile.json",
    ];
    args.extend_from_slice(extra);
    limen(dir, &args)
}

/// Emit a draft and prove limen itself accepts it — the assertion the whole
/// emission path exists to satisfy.
fn draft_and_validate(dir: &Path, extra: &[&str]) -> String {
    let output = suggest(dir, extra);
    assert_eq!(
        code(&output),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let draft = String::from_utf8(output.stdout).expect("utf8 draft");
    std::fs::write(dir.join("draft.yaml"), &draft).expect("write draft");

    let validated = limen(dir, &["validate-config", "-c", "draft.yaml"]);
    assert_eq!(
        code(&validated),
        0,
        "the emitted draft is not a valid limen config:\n{}\n---\n{draft}",
        String::from_utf8_lossy(&validated.stderr)
    );
    draft
}

/// Every comment line in the draft, unwrapped into one whitespace-normalized
/// string, so assertions are about what the comments *say* rather than where
/// the wrapper happened to break a line.
fn comment_text(draft: &str) -> String {
    let text: String = draft
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix('#'))
        .collect::<Vec<_>>()
        .join(" ");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn routes_of(draft: &str) -> Vec<serde_yaml::Value> {
    let parsed: serde_yaml::Value = serde_yaml::from_str(draft).expect("yaml");
    parsed["routes"].as_sequence().expect("routes").clone()
}

fn route<'a>(routes: &'a [serde_yaml::Value], id: &str) -> &'a serde_yaml::Value {
    routes
        .iter()
        .find(|r| r["id"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("route {id} missing from the draft"))
}

#[test]
fn the_default_draft_validates_and_shadows_nothing() {
    let dir = workspace(HOSTILE_CONFIG, &hostile_profile());
    let draft = draft_and_validate(dir.path(), &[]);
    // Asserted on the parsed document, not on the text: the generated comments
    // legitimately mention `comparison.enabled: true` while explaining what
    // adopting a suggestion would do.
    for r in routes_of(&draft) {
        assert_eq!(
            r["comparison"]["enabled"].as_bool(),
            Some(false),
            "{:?} must not be shadowed by default:\n{draft}",
            r["id"]
        );
    }
    assert!(draft.contains("SUGGESTED: compare_candidate"), "{draft}");
    assert!(draft.contains("SUGGESTED: compare_narrowed"), "{draft}");
    assert!(draft.contains("SUGGESTED: relay_only"), "{draft}");
    // Every configured route survives — one vanishing is a route that stops
    // being proxied.
    assert_eq!(routes_of(&draft).len(), 8);
}

#[test]
fn the_default_narrowed_comment_does_not_invite_a_misleading_hand_edit() {
    // `contracted` keeps its contract in the non-adopted draft, so flipping
    // `enabled: true` by hand yields the contract's semantics (body compared),
    // not the status-only narrowing the comment describes.
    let dir = workspace(HOSTILE_CONFIG, &hostile_profile());
    let draft = draft_and_validate(dir.path(), &[]);
    // Searched over the comment text with its wrapping undone, so a reworded
    // line break cannot silently drop the warning.
    let comments = comment_text(&draft);
    assert!(
        comments.contains("re-run with --adopt-suggestions"),
        "the draft must say how to act on a suggestion:\n{draft}"
    );
    assert!(
        comments.contains("leaves this route's contract in force, so the body IS compared"),
        "a hand-flipped contracted route must be called out:\n{draft}"
    );
}

#[test]
fn the_adopted_draft_validates_and_enables_only_the_suggested_routes() {
    let dir = workspace(HOSTILE_CONFIG, &hostile_profile());
    let draft = draft_and_validate(dir.path(), &["--adopt-suggestions"]);
    let routes = routes_of(&draft);

    // The candidate is enabled at limen's own defaults — not at the 1048576 the
    // input route had hand-tuned, which is a consumer's value, not the tool's.
    let candidate = route(&routes, "pat-validate");
    assert_eq!(candidate["comparison"]["enabled"].as_bool(), Some(true));
    assert_eq!(candidate["comparison"]["sample_rate"].as_f64(), Some(1.0));
    assert_eq!(
        candidate["comparison"]["max_body_bytes"].as_u64(),
        Some(262_144)
    );
    // Wholesale replacement: both of these are startup-refusing on a disabled
    // route, and carrying them is the bug this test exists to catch.
    assert!(candidate["comparison"]["shadow_methods"].is_null());
    assert!(candidate["comparison"]["min_comparisons"].is_null());

    // The narrowed route inlines "status yes, body no" — and therefore had to
    // give up its contract reference.
    let narrowed = route(&routes, "contracted");
    assert_eq!(narrowed["comparison"]["enabled"].as_bool(), Some(true));
    assert_eq!(
        narrowed["comparison"]["compare_status"].as_bool(),
        Some(true)
    );
    assert_eq!(
        narrowed["comparison"]["compare_body"].as_bool(),
        Some(false)
    );
    assert!(narrowed["contract"].is_null(), "{draft}");
    // Narrowing must not switch a dimension ON that plain comparison leaves off.
    assert!(narrowed["comparison"]["set_cookie"].is_null());
    assert!(narrowed["comparison"]["location"].is_null());

    // Config-derived relay rules bite even on candidate-shaped traffic.
    for id in ["catchall", "verifier-hop"] {
        assert_eq!(
            route(&routes, id)["comparison"]["enabled"].as_bool(),
            Some(false),
            "{id}"
        );
    }
}

#[test]
fn the_hard_modes_keep_their_client_upstream_selection() {
    // Re-pointing any of these at a legacy primary would move live traffic —
    // a behavior change, not a reformat. Each keeps its mode and stays
    // uncompared despite candidate-shaped traffic.
    let dir = workspace(HOSTILE_CONFIG, &hostile_profile());
    let draft = draft_and_validate(dir.path(), &["--adopt-suggestions"]);
    let routes = routes_of(&draft);
    for (id, mode) in [
        ("split", "percentage_split"),
        ("failover", "failover_to_legacy"),
        ("migrated", "new_only"),
    ] {
        let r = route(&routes, id);
        assert_eq!(r["mode"].as_str(), Some(mode), "{id}");
        assert_eq!(r["comparison"]["enabled"].as_bool(), Some(false), "{id}");
    }
    // …and the fields those modes require survive with them.
    assert_eq!(
        route(&routes, "split")["rollout"]["percentage_flag"].as_str(),
        Some("split.percentage")
    );
    assert_eq!(
        route(&routes, "failover")["failover_safe"].as_bool(),
        Some(true)
    );
    // The query-conditioned pair stays provably disjoint, which is what lets
    // the draft load at all (`validate_query_disjointness`).
    assert_eq!(
        route(&routes, "verifier-hop")["match"]["query_present"][0].as_str(),
        Some("login_verifier")
    );
    assert_eq!(
        route(&routes, "oauth2-auth")["match"]["query_absent"][0].as_str(),
        Some("login_verifier")
    );
}

#[test]
fn a_draft_written_somewhere_else_still_loads() {
    // The expected shape of the command is `suggest-routes -c config/x.yaml >
    // /tmp/draft.yaml`, and every relative path in a config resolves against
    // something that move changes.
    let dir = workspace(HOSTILE_CONFIG, &hostile_profile());
    let output = suggest(dir.path(), &[]);
    assert_eq!(code(&output), 0);
    let draft = String::from_utf8(output.stdout).expect("utf8");

    let elsewhere = tempfile::tempdir().expect("tempdir");
    let relocated = elsewhere.path().join("draft.yaml");
    std::fs::write(&relocated, &draft).expect("write");
    // Validated from a third directory, so neither the input's nor the draft's
    // location is the process CWD.
    let validated = limen(
        Path::new(env!("CARGO_MANIFEST_DIR")),
        &["validate-config", "-c", relocated.to_str().expect("utf8")],
    );
    assert_eq!(
        code(&validated),
        0,
        "a relocated draft must still load:\n{}\n---\n{draft}",
        String::from_utf8_lossy(&validated.stderr)
    );
}

#[test]
fn a_profile_nobody_drove_traffic_through_exits_twenty() {
    let dir = workspace(HOSTILE_CONFIG, &profile_document(1.0, vec![]));
    let output = suggest(dir.path(), &[]);
    assert_eq!(code(&output), 20);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("nothing was profiled"), "{stderr}");
    // A draft is still emitted: it is a correct relay-only draft of this
    // config, and the exit code is what says it rests on nothing.
    assert!(String::from_utf8_lossy(&output.stdout).contains("routes:"));
}

#[test]
fn reads_below_the_floor_are_also_exit_twenty() {
    // No catch-all route here: R1 is config-derived and outranks R2/R3, so a
    // `/` route reports `catch-all` even when nothing was observed on it — and
    // a config whose routes all report a *real* reason exits 0 by design.
    let dir = workspace(
        r#"
observe: {}
routes:
  - id: pat-validate
    match: { methods: ["GET"], path_prefix: "/api/v1/pat/validate" }
    legacy_upstream: "http://legacy.internal"
    mode: legacy_only
"#,
        &profile_document(
            1.0,
            vec![(
                "pat-validate",
                stable_reads(2, "prefix:/api/v1/pat/validate"),
            )],
        ),
    );
    assert_eq!(code(&suggest(dir.path(), &[])), 20);
}

#[test]
fn an_unreadable_profile_is_exit_fifty() {
    let dir = workspace(HOSTILE_CONFIG, &hostile_profile());
    let output = limen(
        dir.path(),
        &[
            "suggest-routes",
            "-c",
            "limen.config.yaml",
            "--profile",
            "no-such-profile.json",
        ],
    );
    assert_eq!(code(&output), 50);
    assert!(String::from_utf8_lossy(&output.stderr).contains("no-such-profile.json"));
}

#[test]
fn a_partially_written_profile_is_exit_fifty() {
    // Structurally valid JSON with every danger signal simply absent. Reading
    // it as a pristine route is the failure this strictness exists to prevent:
    // deny-unknown does not deny-missing.
    let dir = workspace(
        HOSTILE_CONFIG,
        r#"{"sample_rate":1.0,"routes":{"pat-validate":{"observations":34,"reads":34,"length_repeats":12}}}"#,
    );
    let output = suggest(dir.path(), &[]);
    assert_eq!(
        code(&output),
        50,
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn an_unreachable_control_plane_is_exit_fifty() {
    let dir = workspace(HOSTILE_CONFIG, &hostile_profile());
    // Port 1 on loopback: nothing is listening, and the failure is a refused
    // connection rather than a timeout.
    let output = limen(
        dir.path(),
        &[
            "suggest-routes",
            "-c",
            "limen.config.yaml",
            "--control-url",
            "http://127.0.0.1:1",
        ],
    );
    assert_eq!(code(&output), 50);
    assert!(String::from_utf8_lossy(&output.stderr).contains("unreachable"));
}

#[test]
fn a_config_that_never_asked_to_observe_is_exit_fifty() {
    // Not the profiled proxy's config: that proxy's profile endpoint would
    // 404, so this config cannot be the one describing the recorded traffic.
    let dir = workspace(
        &HOSTILE_CONFIG.replace("observe: {}", ""),
        &hostile_profile(),
    );
    let output = suggest(dir.path(), &[]);
    assert_eq!(code(&output), 50);
    assert!(String::from_utf8_lossy(&output.stderr).contains("observe"));
}

#[test]
fn a_config_whose_sample_rate_contradicts_the_profile_is_exit_fifty() {
    // The rate is read off the profile, so a config claiming full coverage
    // cannot talk a sampled profile past R0 — it is rejected as the wrong
    // config instead.
    let dir = workspace(
        HOSTILE_CONFIG, // declares the default sample_rate 1.0
        &profile_document(
            0.25,
            vec![(
                "pat-validate",
                stable_reads(34, "prefix:/api/v1/pat/validate"),
            )],
        ),
    );
    let output = suggest(dir.path(), &[]);
    assert_eq!(code(&output), 50);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("0.25") && stderr.contains("sample_rate"),
        "{stderr}"
    );
}

#[test]
fn a_sampled_profile_classifies_nothing_at_all() {
    // Every route lands on relay_only/partial-sample — R0 refused to classify
    // any of them — so this is the exit-20 case: a draft nobody's traffic
    // informed is not evidence, and automation must not read it as a
    // successful classification.
    let dir = workspace(
        &HOSTILE_CONFIG.replace("observe: {}", "observe: { sample_rate: 0.25 }"),
        &profile_document(
            0.25,
            vec![
                (
                    "pat-validate",
                    stable_reads(34, "prefix:/api/v1/pat/validate"),
                ),
                ("catchall", stable_reads(9, "prefix:/")),
            ],
        ),
    );
    let output = suggest(dir.path(), &["--format", "json"]);
    assert_eq!(code(&output), 20);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json report");
    for entry in report.as_array().expect("array") {
        assert_eq!(entry["disposition"], "relay_only");
        assert_eq!(entry["reason"], "partial-sample");
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("nothing was profiled"), "{stderr}");
    assert!(
        stderr.contains("no route reached compare_candidate"),
        "{stderr}"
    );
}

#[test]
fn the_json_surface_carries_every_matched_narrowing_rule() {
    let dir = workspace(HOSTILE_CONFIG, &hostile_profile());
    let output = suggest(dir.path(), &["--format", "json"]);
    assert_eq!(code(&output), 0);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json report");
    let narrowed = report
        .as_array()
        .expect("array")
        .iter()
        .find(|e| e["route_id"] == "contracted")
        .expect("contracted");
    assert_eq!(narrowed["disposition"], "compare_narrowed");
    assert_eq!(narrowed["reason"], "stability-unobserved");
    // `reason` is the first match; the evidence keeps the rest.
    assert_eq!(
        narrowed["evidence"]["narrowing_matches"],
        serde_json::json!(["stability-unobserved"])
    );
    assert_eq!(narrowed["evidence"]["length_missing"], 3);
}

#[test]
fn a_route_that_only_ever_failed_is_relayed_and_says_why() {
    // End to end on the published vocabulary: an all-4xx corpus that is
    // innocuous on every other axis reaches the CLI's own surfaces — the JSON
    // reason downstream harnesses grep for, and the draft comment a human
    // reads — rather than being drafted as a candidate on the stability of an
    // error page. `--adopt-suggestions` must not enable it either.
    let mut routes = hostile_routes();
    let (_, migrated) = routes
        .iter_mut()
        .find(|(id, _)| *id == "migrated")
        .expect("migrated route");
    migrated.status_classes = BTreeMap::from([("4xx".to_string(), migrated.reads)]);
    // …and no stability evidence, because a success-qualified recorder accrues
    // none from an error response. Stating it here is not bookkeeping: a
    // document claiming repeats behind zero successes is refused at the
    // `--profile` door as arithmetically impossible, so it would never reach
    // the classifier this test is about. R8a is still what catches the route —
    // it is a relay rule, so it is decided before R10 ever sees the zero.
    migrated.length_repeats = 0;
    migrated.length_varied = 0;
    migrated.length_missing = 0;
    let dir = workspace(HOSTILE_CONFIG, &profile_document(1.0, routes));

    let output = suggest(dir.path(), &["--format", "json"]);
    assert_eq!(code(&output), 0);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json report");
    let entry = report
        .as_array()
        .expect("array")
        .iter()
        .find(|e| e["route_id"] == "migrated")
        .expect("migrated");
    assert_eq!(entry["disposition"], "relay_only");
    assert_eq!(entry["reason"], "no-success-evidence");

    let draft = draft_and_validate(dir.path(), &["--adopt-suggestions"]);
    let comments = comment_text(&draft);
    assert!(
        comments.contains("relay_only (no-success-evidence)"),
        "{comments}"
    );
    assert!(comments.contains("no read ever succeeded"), "{comments}");
    let routes = routes_of(&draft);
    assert_eq!(
        route(&routes, "migrated")["comparison"]["enabled"],
        serde_yaml::Value::Bool(false),
        "adoption must not promote a route that has never worked"
    );
}

#[test]
fn adopt_says_nothing_about_a_json_run() {
    // `--adopt-suggestions` changes nothing about the machine surface, so a
    // note claiming comparison was enabled would describe a document that does
    // not exist.
    let dir = workspace(HOSTILE_CONFIG, &hostile_profile());
    let output = suggest(dir.path(), &["--format", "json", "--adopt-suggestions"]);
    assert_eq!(code(&output), 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("enabled: true"), "{stderr}");

    let yaml = suggest(dir.path(), &["--adopt-suggestions"]);
    assert!(String::from_utf8_lossy(&yaml.stderr).contains("enabled: true"));
}

/// A config whose one route is a template, plus the profile a proxy running it
/// would have written: one shape, however many conversations it served.
const TEMPLATED_CONFIG: &str = r#"
observe: {}
routes:
  - id: conversation
    match: { methods: ["GET"], path_template: "/conversations/{id}" }
    legacy_upstream: "http://legacy.internal"
    mode: legacy_only
"#;

fn templated_profile(basis: &str) -> String {
    profile_document(1.0, vec![("conversation", stable_reads(40, basis))])
}

#[test]
fn a_templated_route_adopts_into_a_config_limen_accepts() {
    // The end of the line for a templated route: classified, drafted, adopted,
    // and then loaded back by limen itself. The match block has to survive that
    // round trip verbatim or the adopted config proxies something else.
    let dir = workspace(
        TEMPLATED_CONFIG,
        &templated_profile("template:/conversations/{id}"),
    );
    let draft = draft_and_validate(
        dir.path(),
        &[
            "--adopt-suggestions",
            "--new-upstream",
            "http://new.internal",
        ],
    );
    let routes = routes_of(&draft);
    assert_eq!(
        route(&routes, "conversation")["match"]["path_template"].as_str(),
        Some("/conversations/{id}")
    );
    assert!(route(&routes, "conversation")["match"]["path_prefix"].is_null());
    assert_eq!(
        route(&routes, "conversation")["mode"].as_str(),
        Some("shadow_legacy_primary")
    );
    assert_eq!(
        route(&routes, "conversation")["comparison"]["enabled"].as_bool(),
        Some(true)
    );
    // The count the template folded is labelled as folded, so "1 path" on a
    // route that served forty conversations cannot be misread.
    assert!(
        comment_text(&draft).contains("1 path (template-normalized)"),
        "{draft}"
    );
}

#[test]
fn a_profile_recorded_under_another_matcher_is_exit_fifty() {
    // The stale-profile case end to end: the route was templated after this
    // profile was taken, so its per-id path spread would now read as one tidy
    // endpoint. Refused rather than reinterpreted.
    let dir = workspace(
        TEMPLATED_CONFIG,
        &templated_profile("prefix:/conversations/"),
    );
    let output = suggest(dir.path(), &[]);
    assert_eq!(
        code(&output),
        50,
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for named in [
        "conversation",
        "prefix:/conversations/",
        "/conversations/{id}",
    ] {
        assert!(stderr.contains(named), "{stderr}");
    }
}

#[test]
fn a_route_with_no_new_upstream_drafts_as_legacy_only() {
    let dir = workspace(
        r#"
observe: {}
routes:
  - id: only-legacy
    match: { methods: ["GET"], path_prefix: "/api" }
    legacy_upstream: "http://legacy.internal"
    mode: legacy_only
"#,
        &profile_document(1.0, vec![("only-legacy", stable_reads(9, "prefix:/api"))]),
    );
    let draft = draft_and_validate(dir.path(), &["--adopt-suggestions"]);
    let routes = routes_of(&draft);
    assert_eq!(routes[0]["mode"].as_str(), Some("legacy_only"));
    assert_eq!(routes[0]["comparison"]["enabled"].as_bool(), Some(false));
    assert!(draft.contains("--new-upstream"), "{draft}");

    // With one supplied, the same profile drafts the shadowing form.
    let draft = draft_and_validate(
        dir.path(),
        &[
            "--adopt-suggestions",
            "--new-upstream",
            "http://new.internal",
        ],
    );
    let routes = routes_of(&draft);
    assert_eq!(routes[0]["mode"].as_str(), Some("shadow_legacy_primary"));
    assert_eq!(routes[0]["comparison"]["enabled"].as_bool(), Some(true));
}
