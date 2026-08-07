//! `limen verdict`: render a campaign verdict from three limen-owned inputs —
//! the configuration (route matrix + floors), the live control plane's
//! `/metrics`, and the diff-sink directory.
//!
//! This is the operator-checked gate the spec sketches in §12.1, built for
//! migration campaigns: after driving traffic through the proxy, one command
//! answers "did the comparison pipeline actually bite, and what did it find?"
//! with typed exit codes a wrapper can branch on instead of parsing prose.
//!
//! Fail-closed doctrine (every decision here bends toward it):
//!
//! - **Nothing is inferred from absence.** A required series missing from the
//!   scrape, an unreachable control plane, an unreadable sink, a refused
//!   canary trigger — each is [`VerdictError::InputUnavailable`] (exit 50),
//!   never "0".
//! - **Drain is observed, not slept.** The pipeline is drained only when
//!   `limen_shadow_in_flight == 0` and every offered sink record is accounted
//!   for (`enqueued == written + dropped`) across **two consecutive,
//!   value-identical scrapes**. Two scrapes because the Prometheus exporter
//!   snapshots counters before gauges within one render, so a single scrape
//!   can read balanced+idle while a record offered mid-render is still
//!   unwritten; stability across two closes that tear once traffic has
//!   stopped (the documented precondition for running a verdict).
//! - **A verdict that verified nothing is a failure.** Every enabled route
//!   with a non-zero `comparison.min_comparisons` must have recorded at least
//!   that many comparisons, and a config in which no route is floored fails
//!   the floors check outright.
//! - **The sink must agree with the engine, per route.** Counters increment
//!   when a comparison completes; the sink is an async pipeline that can drop
//!   (and says so in its own counters). Any disagreement — drops, torn lines,
//!   per-route count divergence, or a counter route the config has never
//!   heard of (a config edited after start, or a scrape of the wrong
//!   instance) — is a typed integrity failure.
//!
//! The exit-code vocabulary (documented in the CLI reference, chosen to
//! collide with neither anyhow's 1 nor clap's 2): 0 clean, 10 mismatches
//! found, 20 floors unmet, 30 sink integrity, 40 drain timeout, 50 input
//! unavailable. When several conditions hold the highest code wins — the
//! worse tooling condition dominates, because it makes the lower-numbered
//! answers untrustworthy.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tokio::time::Instant;

use crate::config::model::Config;
use crate::observability::prometheus::{
    COMPARISONS_TOTAL, COMPARISON_SKIPPED_TOTAL, DIFF_SINK_DROPPED_TOTAL, DIFF_SINK_ENQUEUED_TOTAL,
    DIFF_SINK_WRITTEN_TOTAL, SHADOW_FAILED_TOTAL, SHADOW_IN_FLIGHT, SHADOW_SKIPPED_TOTAL,
};
use crate::observability::sink::{self, Report, ReportFilter, REPORT_EXAMPLES_PER_ROUTE};

/// The route id the debug sink canary records under. Reserved: config
/// validation rejects user routes in the reserved namespace, so a canary
/// record can never be confused with a real mismatch.
pub const CANARY_ROUTE_ID: &str = "__limen_canary__";

/// Route ids starting with this prefix are reserved for limen-internal
/// records (today: the sink canary).
pub const RESERVED_ROUTE_ID_PREFIX: &str = "__";

/// The series whose values this verdict's math rests on: watched for
/// stability by the drain loop and validated as exact integers before any
/// comparison trusts them.
const WATCHED_SERIES: [&str; 5] = [
    SHADOW_IN_FLIGHT,
    DIFF_SINK_ENQUEUED_TOTAL,
    DIFF_SINK_WRITTEN_TOTAL,
    DIFF_SINK_DROPPED_TOTAL,
    COMPARISONS_TOTAL,
];

// ---------------------------------------------------------------------------
// Outcomes and errors
// ---------------------------------------------------------------------------

/// The verdict outcome, ordered by severity: when several conditions hold the
/// maximum wins (`Ord` follows declaration order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VerdictCode {
    /// Drained, floors met, sink integral, zero non-canary mismatches.
    Clean,
    /// Non-canary mismatches were recorded (and the pipeline is trustworthy).
    Mismatches,
    /// At least one floored route compared fewer times than its floor — or
    /// the config floors nothing at all.
    FloorsUnmet,
    /// The sink and the engine's counters disagree: drops, torn lines,
    /// per-route divergence, an unknown counter route, or a bad canary.
    SinkIntegrity,
    /// The pipeline never quiesced within the deadline.
    DrainTimeout,
}

impl VerdictCode {
    /// The documented process exit code.
    pub fn exit_code(self) -> u8 {
        match self {
            VerdictCode::Clean => 0,
            VerdictCode::Mismatches => 10,
            VerdictCode::FloorsUnmet => 20,
            VerdictCode::SinkIntegrity => 30,
            VerdictCode::DrainTimeout => 40,
        }
    }

    /// The stable machine-readable name used in JSON output.
    pub fn name(self) -> &'static str {
        match self {
            VerdictCode::Clean => "clean",
            VerdictCode::Mismatches => "mismatches-found",
            VerdictCode::FloorsUnmet => "floors-unmet",
            VerdictCode::SinkIntegrity => "sink-integrity-failure",
            VerdictCode::DrainTimeout => "drain-timeout",
        }
    }
}

/// Exit code for a verdict whose required inputs were unavailable.
pub const EXIT_INPUT_UNAVAILABLE: u8 = 50;

/// A required input was unavailable or unusable. Typed so the CLI exits 50 —
/// distinguishable from every real verdict and from untyped tool errors.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct InputUnavailable(pub String);

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Resolved inputs for one verdict run (the CLI derives these from the config
/// file plus flags).
#[derive(Debug, Clone)]
pub struct VerdictOptions {
    /// The diff-sink directory to read.
    pub sink_dir: PathBuf,
    /// Control-plane base URL, e.g. `http://127.0.0.1:9090`.
    pub control_base: String,
    /// Path the Prometheus exposition is served on (from `metrics.path`).
    pub metrics_path: String,
    /// Trigger the debug sink canary and require it end-to-end.
    pub canary: bool,
    /// Degraded report-only mode: no drain, floors, integrity, or canary.
    pub offline: bool,
    /// How long the drain loop may wait for quiescence.
    pub drain_deadline: Duration,
    /// Interval between drain scrapes.
    pub poll_interval: Duration,
}

// ---------------------------------------------------------------------------
// Prometheus exposition parsing
// ---------------------------------------------------------------------------

/// One parsed exposition sample.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub value: f64,
}

/// A parsed `/metrics` scrape.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scrape {
    samples: Vec<Sample>,
}

impl Scrape {
    /// Parse a Prometheus text exposition. Comment/blank lines are skipped;
    /// any other unparseable line is an error — this parser reads limen's own
    /// exporter, so leniency would only hide renderer drift (fail closed).
    pub fn parse(text: &str) -> Result<Scrape, InputUnavailable> {
        let mut samples = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let sample = parse_sample(line)
                .ok_or_else(|| InputUnavailable(format!("unparseable /metrics line: {line:?}")))?;
            samples.push(sample);
        }
        Ok(Scrape { samples })
    }

    /// All samples of a metric family.
    fn family<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Sample> + 'a {
        self.samples.iter().filter(move |s| s.name == name)
    }

    /// Whether the family has at least one sample (zero-registered counts).
    pub fn has_family(&self, name: &str) -> bool {
        self.family(name).next().is_some()
    }

    /// Sum a family's samples, optionally constrained to label subset matches.
    /// `None` when the family is entirely absent — callers must decide what
    /// absence means; this module never silently reads it as zero.
    pub fn sum(&self, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        let mut any = false;
        let mut total = 0.0;
        for s in self.family(name) {
            any = true;
            if labels
                .iter()
                .all(|(k, v)| s.labels.get(*k).map(String::as_str) == Some(*v))
            {
                total += s.value;
            }
        }
        any.then_some(total)
    }

    /// Distinct values of a label across a family.
    pub fn label_values(&self, name: &str, label: &str) -> BTreeSet<String> {
        self.family(name)
            .filter_map(|s| s.labels.get(label).cloned())
            .collect()
    }

    /// The subset of samples the drain loop watches, in comparable form.
    /// Restricted to the comparison pipeline's own series: request-path noise
    /// (latency histograms, client gauges) must not keep a quiescent pipeline
    /// reading "unstable" forever.
    fn stable_view(&self) -> BTreeMap<(String, String), f64> {
        self.samples
            .iter()
            .filter(|s| WATCHED_SERIES.contains(&s.name.as_str()))
            .map(|s| {
                let labels = s
                    .labels
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(",");
                ((s.name.clone(), labels), s.value)
            })
            .collect()
    }
}

/// Parse one `name{label="value",...} value` (or `name value`) line.
fn parse_sample(line: &str) -> Option<Sample> {
    let (head, value) = line.rsplit_once(' ')?;
    let value: f64 = value.trim().parse().ok()?;
    let (name, labels) = match head.split_once('{') {
        None => (head.trim().to_string(), BTreeMap::new()),
        Some((name, rest)) => {
            let rest = rest.trim_end();
            let body = rest.strip_suffix('}')?;
            (name.trim().to_string(), parse_labels(body)?)
        }
    };
    if name.is_empty() {
        return None;
    }
    Some(Sample {
        name,
        labels,
        value,
    })
}

/// Parse an exposition label body (`k="v",k2="v2"`), honoring the format's
/// `\\`, `\"`, and `\n` escapes.
fn parse_labels(body: &str) -> Option<BTreeMap<String, String>> {
    let mut labels = BTreeMap::new();
    let mut chars = body.chars().peekable();
    loop {
        // Skip separators; done at end of body.
        while matches!(chars.peek(), Some(',') | Some(' ')) {
            chars.next();
        }
        if chars.peek().is_none() {
            return Some(labels);
        }
        let mut key = String::new();
        for c in chars.by_ref() {
            if c == '=' {
                break;
            }
            key.push(c);
        }
        if chars.next() != Some('"') {
            return None;
        }
        let mut value = String::new();
        loop {
            match chars.next()? {
                '"' => break,
                '\\' => match chars.next()? {
                    'n' => value.push('\n'),
                    c => value.push(c),
                },
                c => value.push(c),
            }
        }
        labels.insert(key.trim().to_string(), value);
    }
}

// ---------------------------------------------------------------------------
// Checks (pure — everything here is unit-testable without a server)
// ---------------------------------------------------------------------------

/// A single check's outcome, as reported in both output formats.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub status: CheckStatus,
    pub detail: String,
}

/// Check status vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail,
    Skipped,
}

impl Check {
    fn pass(detail: impl Into<String>) -> Self {
        Check {
            status: CheckStatus::Pass,
            detail: detail.into(),
        }
    }
    fn fail(detail: impl Into<String>) -> Self {
        Check {
            status: CheckStatus::Fail,
            detail: detail.into(),
        }
    }
    fn skipped(detail: impl Into<String>) -> Self {
        Check {
            status: CheckStatus::Skipped,
            detail: detail.into(),
        }
    }
}

/// One floored route's standing.
#[derive(Debug, Clone, Serialize)]
pub struct RouteFloor {
    pub route_id: String,
    pub comparisons: u64,
    pub floor: u64,
    pub met: bool,
}

/// An informational (non-gating) counter surfaced for inspection: shadows and
/// comparisons that were skipped or failed. Gating these is deliberately
/// staged for a later phase; a verdict still shows them so starvation causes
/// are diagnosable from its output alone.
#[derive(Debug, Clone, Serialize)]
pub struct InfoCounter {
    pub metric: String,
    pub route: String,
    pub reason: String,
    pub value: u64,
}

/// How the drain phase ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainStatus {
    /// Two consecutive stable, balanced scrapes.
    Drained,
    /// The deadline elapsed before quiescence.
    TimedOut,
    /// More records accounted out of the queue than were ever offered —
    /// counters are corrupt or the scrape hit a different process. An
    /// immediate integrity failure, never a wait state.
    OverBalanced,
    /// `--offline`: drain not attempted.
    Offline,
}

/// The full verdict, serializable as the `--format json` document.
#[derive(Debug, Clone, Serialize)]
pub struct VerdictReport {
    pub mode: &'static str,
    pub verdict: &'static str,
    pub exit_code: u8,
    pub checks: Checks,
    /// Non-canary mismatch records in the sink.
    pub mismatches_total: u64,
    /// Canary records in the sink (excluded from `mismatches_total`).
    pub canary_records: u64,
    pub floors: Vec<RouteFloor>,
    /// Per-route mismatch counts from the sink (non-canary), for wrappers
    /// that render summaries without re-reading the sink.
    pub sink_mismatches_by_route: BTreeMap<String, u64>,
    pub informational: Vec<InfoCounter>,
}

/// The named checks, in report order.
#[derive(Debug, Clone, Serialize)]
pub struct Checks {
    pub drain: Check,
    pub floors: Check,
    pub sink_integrity: Check,
    pub canary: Check,
    pub mismatches: Check,
}

impl DrainStatus {
    /// The code this outcome contributes. Over-balance is an integrity
    /// failure, not a wait state: nothing longer would fix corrupt counters.
    fn code(self) -> VerdictCode {
        match self {
            DrainStatus::Drained | DrainStatus::Offline => VerdictCode::Clean,
            DrainStatus::OverBalanced => VerdictCode::SinkIntegrity,
            DrainStatus::TimedOut => VerdictCode::DrainTimeout,
        }
    }
}

impl VerdictReport {
    /// Recomputed from the checks (plus the drain outcome, whose two failure
    /// modes carry different codes) so the fields can't drift from them.
    /// Highest code wins: the worse condition makes the lower ones untrustworthy.
    fn code(&self, drain: DrainStatus) -> VerdictCode {
        let mut code = drain.code();
        if self.checks.mismatches.status == CheckStatus::Fail {
            code = code.max(VerdictCode::Mismatches);
        }
        if self.checks.floors.status == CheckStatus::Fail {
            code = code.max(VerdictCode::FloorsUnmet);
        }
        if self.checks.sink_integrity.status == CheckStatus::Fail
            || self.checks.canary.status == CheckStatus::Fail
        {
            code = code.max(VerdictCode::SinkIntegrity);
        }
        code
    }
}

/// Evaluate every check over the final scrape and the sink report. Pure: the
/// drain loop and all IO happen in [`run_verdict`]; this function is the
/// entire decision surface, so tests can drive it with synthetic inputs.
pub fn evaluate(
    config: &Config,
    scrape: &Scrape,
    report: &Report,
    canary_requested: bool,
    drain: DrainStatus,
) -> VerdictReport {
    let canary_records = report
        .routes
        .iter()
        .filter(|r| r.route_id == CANARY_ROUTE_ID)
        .map(|r| r.count as u64)
        .sum::<u64>();
    let mismatches_total = report.total as u64 - canary_records;
    let sink_mismatches_by_route: BTreeMap<String, u64> = report
        .routes
        .iter()
        .filter(|r| !r.route_id.starts_with(RESERVED_ROUTE_ID_PREFIX))
        .map(|r| (r.route_id.clone(), r.count as u64))
        .collect();

    let drain_check = match drain {
        DrainStatus::Drained => Check::pass("pipeline quiesced (two stable balanced scrapes)"),
        DrainStatus::TimedOut => Check::fail(
            "pipeline did not quiesce within the deadline; counts below are untrustworthy",
        ),
        DrainStatus::OverBalanced => Check::fail(
            "over-balance: more sink records written+dropped than were ever offered — \
             corrupt counters or a scrape of a different process",
        ),
        DrainStatus::Offline => Check::skipped("offline mode: drain not attempted"),
    };

    let offline = drain == DrainStatus::Offline;
    let (floors_check, floors, integrity_check, canary_check) = if offline {
        (
            Check::skipped("offline mode: no metrics to floor against"),
            Vec::new(),
            Check::skipped("offline mode: no counters to reconcile against"),
            Check::skipped("offline mode: canary not triggered"),
        )
    } else {
        let (floors_check, floors) = evaluate_floors(config, scrape);
        (
            floors_check,
            floors,
            evaluate_integrity(config, scrape, report),
            evaluate_canary(scrape, canary_records, canary_requested),
        )
    };

    let mismatches_check = if mismatches_total == 0 {
        Check::pass("zero non-canary mismatches recorded")
    } else {
        Check::fail(format!(
            "{mismatches_total} non-canary mismatch(es) recorded — inspect `limen report`"
        ))
    };

    let mut verdict = VerdictReport {
        mode: if offline { "offline" } else { "online" },
        verdict: VerdictCode::Clean.name(),
        exit_code: 0,
        checks: Checks {
            drain: drain_check,
            floors: floors_check,
            sink_integrity: integrity_check,
            canary: canary_check,
            mismatches: mismatches_check,
        },
        mismatches_total,
        canary_records,
        floors,
        sink_mismatches_by_route,
        informational: collect_informational(scrape),
    };
    let code = verdict.code(drain);
    verdict.verdict = code.name();
    verdict.exit_code = code.exit_code();
    verdict
}

/// The floors check: every enabled route with a non-zero floor must have
/// recorded at least that many comparisons; a config that floors nothing
/// fails outright (a verdict over it would prove nothing).
fn evaluate_floors(config: &Config, scrape: &Scrape) -> (Check, Vec<RouteFloor>) {
    let floored: Vec<&crate::config::model::RouteConfig> = config
        .routes
        .iter()
        .filter(|r| r.comparison.enabled && r.comparison.min_comparisons > 0)
        .collect();
    if floored.is_empty() {
        return (
            Check::fail(
                "no route is both comparison-enabled and floored — this config compares \
                 nothing a verdict could vouch for",
            ),
            Vec::new(),
        );
    }
    let mut rows = Vec::with_capacity(floored.len());
    let mut starved = Vec::new();
    for route in floored {
        // Absent-from-scrape deliberately reads as 0 here: with a floor >= 1
        // that is fail-closed (the whole point of the floor).
        let comparisons = scrape
            .sum(COMPARISONS_TOTAL, &[("route", &route.id)])
            .unwrap_or(0.0) as u64;
        let floor = route.comparison.min_comparisons;
        let met = comparisons >= floor;
        if !met {
            starved.push(route.id.clone());
        }
        rows.push(RouteFloor {
            route_id: route.id.clone(),
            comparisons,
            floor,
            met,
        });
    }
    let check = if starved.is_empty() {
        Check::pass(format!(
            "{} floored route(s) all at/above floor",
            rows.len()
        ))
    } else {
        Check::fail(format!(
            "route(s) below their comparison floor: {} — a route that never compared \
             cannot contribute a mismatch, so a clean total proves nothing about it",
            starved.join(", ")
        ))
    };
    (check, rows)
}

/// The sink-integrity check: drops, torn lines, per-route counter/sink
/// divergence, and counter routes the config does not declare.
fn evaluate_integrity(config: &Config, scrape: &Scrape, report: &Report) -> Check {
    let mut problems = Vec::new();

    // Sink drops: every dropped record is a mismatch the report cannot show.
    let dropped = scrape.sum(DIFF_SINK_DROPPED_TOTAL, &[]).unwrap_or(0.0);
    if dropped > 0.0 {
        problems.push(format!("{dropped} sink record(s) dropped"));
    }

    if report.malformed_lines > 0 {
        problems.push(format!(
            "{} unparseable sink line(s) — the pipeline was interrupted mid-write",
            report.malformed_lines
        ));
    }

    // Counter routes the config has never heard of: a config edited after the
    // proxy started, or a scrape aimed at a different limen instance.
    let known: BTreeSet<&str> = config.routes.iter().map(|r| r.id.as_str()).collect();
    let unknown: Vec<String> = scrape
        .label_values(COMPARISONS_TOTAL, "route")
        .into_iter()
        .filter(|r| !r.starts_with(RESERVED_ROUTE_ID_PREFIX) && !known.contains(r.as_str()))
        .collect();
    if !unknown.is_empty() {
        problems.push(format!(
            "counter route(s) not in this config: {} — config drift or wrong instance",
            unknown.join(", ")
        ));
    }

    // Per-route reconciliation, canary route included symmetrically: every
    // route either side knows about must agree exactly. Compensating errors
    // across routes cannot hide in a per-route comparison.
    let mut route_ids: BTreeSet<String> = scrape.label_values(COMPARISONS_TOTAL, "route");
    for r in &report.routes {
        route_ids.insert(r.route_id.clone());
    }
    for route_id in route_ids {
        let counted = scrape
            .sum(
                COMPARISONS_TOTAL,
                &[("route", &route_id), ("result", "mismatch")],
            )
            .unwrap_or(0.0) as u64;
        let sunk = report
            .routes
            .iter()
            .find(|r| r.route_id == route_id)
            .map(|r| r.count as u64)
            .unwrap_or(0);
        if counted != sunk {
            problems.push(format!(
                "route {route_id}: engine counted {counted} mismatch(es) but the sink \
                 holds {sunk}"
            ));
        }
    }

    if problems.is_empty() {
        Check::pass("sink and engine counters agree on every route; nothing dropped")
    } else {
        Check::fail(problems.join("; "))
    }
}

/// The canary check. Relative rather than "exactly one" — the sink record
/// count must equal the engine's canary mismatch counter AND be at least one
/// — so a verdict stays re-runnable against a live proxy across sink resets
/// while still failing on a dropped or double-counted canary.
fn evaluate_canary(scrape: &Scrape, canary_records: u64, requested: bool) -> Check {
    if !requested {
        return if canary_records > 0 {
            Check::pass(format!(
                "{canary_records} canary record(s) present (excluded from totals); \
                 --canary not requested"
            ))
        } else {
            Check::skipped("--canary not requested")
        };
    }
    let counted = scrape
        .sum(
            COMPARISONS_TOTAL,
            &[("route", CANARY_ROUTE_ID), ("result", "mismatch")],
        )
        .unwrap_or(0.0) as u64;
    if canary_records == counted && canary_records >= 1 {
        Check::pass(format!(
            "canary rode compare → sink → flush end-to-end ({canary_records} record(s), \
             counters agree)"
        ))
    } else {
        Check::fail(format!(
            "canary integrity: engine counted {counted}, sink holds {canary_records} \
             (require equal and >= 1) — the record→flush→report pipeline did not \
             demonstrably bite"
        ))
    }
}

/// Skip/failure counters surfaced for inspection (non-gating; see
/// [`InfoCounter`]).
fn collect_informational(scrape: &Scrape) -> Vec<InfoCounter> {
    let mut rows = Vec::new();
    for metric in [
        SHADOW_SKIPPED_TOTAL,
        SHADOW_FAILED_TOTAL,
        COMPARISON_SKIPPED_TOTAL,
    ] {
        for s in scrape.family(metric) {
            if s.value == 0.0 {
                continue;
            }
            rows.push(InfoCounter {
                metric: metric.to_string(),
                route: s.labels.get("route").cloned().unwrap_or_default(),
                reason: s.labels.get("reason").cloned().unwrap_or_default(),
                value: s.value as u64,
            });
        }
    }
    rows
}

// ---------------------------------------------------------------------------
// The online run: canary trigger, drain loop, sink read
// ---------------------------------------------------------------------------

/// Series whose presence in the scrape is mandatory: their absence means the
/// instrumentation the whole verdict rests on is missing (a proxy older than
/// the verdict tool, or a renderer regression) — never "zero events".
const REQUIRED_SERIES: [&str; 4] = [
    SHADOW_IN_FLIGHT,
    DIFF_SINK_ENQUEUED_TOTAL,
    DIFF_SINK_WRITTEN_TOTAL,
    DIFF_SINK_DROPPED_TOTAL,
];

/// Run the full verdict. `Err` is always [`InputUnavailable`] → exit 50.
pub async fn run_verdict(
    config: &Config,
    opts: &VerdictOptions,
) -> Result<VerdictReport, InputUnavailable> {
    if opts.offline {
        let report = read_sink(&opts.sink_dir)?;
        return Ok(evaluate(
            config,
            &Scrape::default(),
            &report,
            false,
            DrainStatus::Offline,
        ));
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| InputUnavailable(format!("cannot build HTTP client: {e}")))?;

    // Trigger the canary BEFORE draining, so its record rides the real async
    // pipeline and the drain wait covers it like any other mismatch.
    if opts.canary {
        trigger_canary(&client, &opts.control_base).await?;
    }

    let (scrape, drain) = drain(&client, opts).await?;
    let report = read_sink(&opts.sink_dir)?;
    Ok(evaluate(config, &scrape, &report, opts.canary, drain))
}

/// POST the debug canary endpoint. Any refusal is exit-50 territory: the
/// injection input was denied, so nothing downstream is trustworthy.
async fn trigger_canary(client: &reqwest::Client, base: &str) -> Result<(), InputUnavailable> {
    let url = format!("{base}/debug/canary");
    let resp = client
        .post(&url)
        .send()
        .await
        .map_err(|e| InputUnavailable(format!("canary trigger unreachable at {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(InputUnavailable(format!(
            "canary trigger refused at {url} (HTTP {}): is `debug.sink_canary: true` set \
             in the running proxy's config?",
            resp.status()
        )));
    }
    // Require the endpoint's own acknowledgment, not just a 2xx: a mis-aimed
    // URL happily returning 200 for any POST must not count as an injection
    // (adversarial review) — only limen's canary answers `"injected": true`.
    let text = resp
        .text()
        .await
        .map_err(|e| InputUnavailable(format!("cannot read canary response from {url}: {e}")))?;
    let body: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        InputUnavailable(format!(
            "canary trigger at {url} returned a non-JSON body: {e}"
        ))
    })?;
    if body.get("injected") != Some(&serde_json::Value::Bool(true)) {
        return Err(InputUnavailable(format!(
            "canary trigger at {url} did not acknowledge the injection (body: {body})"
        )));
    }
    Ok(())
}

/// The drain loop: poll `/metrics` until two consecutive scrapes are both
/// balanced and value-identical over the watched series, or the deadline
/// passes. See the module doc for why one scrape is not enough.
async fn drain(
    client: &reqwest::Client,
    opts: &VerdictOptions,
) -> Result<(Scrape, DrainStatus), InputUnavailable> {
    let deadline = Instant::now() + opts.drain_deadline;
    let mut prev: Option<BTreeMap<(String, String), f64>> = None;
    loop {
        let scrape = fetch_scrape(client, opts).await?;
        for series in REQUIRED_SERIES {
            if !scrape.has_family(series) {
                return Err(InputUnavailable(format!(
                    "required series {series} absent from the scrape — the proxy is not \
                     exporting the verdict instrumentation (older binary?)"
                )));
            }
        }
        // Every count this verdict compares must be an exact integer: past
        // 2^53 an f64 `==` can equate values that differ by one, which is
        // precisely the discrepancy the integrity checks exist to catch
        // (adversarial review). Validated once here so every downstream
        // comparison and cast works on exact values.
        for name in WATCHED_SERIES {
            for sample in scrape.family(name) {
                let v = sample.value;
                if v.fract() != 0.0 || !(0.0..9_007_199_254_740_992.0).contains(&v) {
                    return Err(InputUnavailable(format!(
                        "series {name} carries a non-exact count ({v}) — refusing \
                         float-imprecise integrity math"
                    )));
                }
            }
        }
        // Absence handled above, so these sums are all present.
        let in_flight = scrape.sum(SHADOW_IN_FLIGHT, &[]).unwrap_or(0.0);
        let enqueued = scrape.sum(DIFF_SINK_ENQUEUED_TOTAL, &[]).unwrap_or(0.0);
        let written = scrape.sum(DIFF_SINK_WRITTEN_TOTAL, &[]).unwrap_or(0.0);
        let dropped = scrape.sum(DIFF_SINK_DROPPED_TOTAL, &[]).unwrap_or(0.0);

        if enqueued < written + dropped {
            return Ok((scrape, DrainStatus::OverBalanced));
        }
        let balanced = in_flight == 0.0 && enqueued == written + dropped;
        let view = scrape.stable_view();
        if balanced && prev.as_ref() == Some(&view) {
            return Ok((scrape, DrainStatus::Drained));
        }
        prev = Some(view);
        if Instant::now() >= deadline {
            return Ok((scrape, DrainStatus::TimedOut));
        }
        tokio::time::sleep(opts.poll_interval).await;
    }
}

/// Fetch and parse one `/metrics` scrape.
async fn fetch_scrape(
    client: &reqwest::Client,
    opts: &VerdictOptions,
) -> Result<Scrape, InputUnavailable> {
    let url = format!("{}{}", opts.control_base, opts.metrics_path);
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| InputUnavailable(format!("control plane unreachable at {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(InputUnavailable(format!(
            "control plane returned HTTP {} for {url}",
            resp.status()
        )));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| InputUnavailable(format!("cannot read {url}: {e}")))?;
    Scrape::parse(&text)
}

/// Read the sink report; an unreadable directory is exit-50 territory.
fn read_sink(dir: &Path) -> Result<Report, InputUnavailable> {
    sink::read_report(dir, &ReportFilter::default(), REPORT_EXAMPLES_PER_ROUTE)
        .map_err(|e| InputUnavailable(format!("cannot read sink directory {}: {e}", dir.display())))
}

// ---------------------------------------------------------------------------
// Derivations shared with the CLI
// ---------------------------------------------------------------------------

/// Derive the control-plane base URL from `metrics.listen_addr`, mapping
/// wildcard binds to loopback (a `0.0.0.0:9090` bind is scraped at
/// `127.0.0.1:9090` — the in-container case).
pub fn control_base_from_listen_addr(listen_addr: &str) -> String {
    let (host, port) = listen_addr
        .rsplit_once(':')
        .unwrap_or((listen_addr, "9090"));
    let host = match host {
        "0.0.0.0" | "::" | "[::]" | "" => "127.0.0.1",
        other => other,
    };
    format!("http://{host}:{port}")
}

/// The drain deadline for a config: the longest shadow timeout any route
/// declares (an in-flight shadow can legally live that long) plus slack for
/// compare + sink flush.
pub fn drain_deadline(config: &Config, slack: Duration) -> Duration {
    let max_shadow_ms = config
        .routes
        .iter()
        .map(|r| r.timeouts.shadow_ms)
        .max()
        .unwrap_or(0);
    Duration::from_millis(max_shadow_ms) + slack
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The horizontal rules the human block is framed with.
const RULE: &str = "============================================================";
const THIN_RULE: &str = "------------------------------------------------------------";

/// Render the human-readable verdict block.
pub fn render_human(v: &VerdictReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "{RULE}");
    let _ = writeln!(out, " LIMEN VERDICT ({})", v.mode);
    if v.mode == "offline" {
        let _ = writeln!(
            out,
            " DEGRADED: drain, floors, sink integrity, and canary were NOT checked."
        );
    }
    let _ = writeln!(out, "{RULE}");
    for (name, check) in [
        ("drain", &v.checks.drain),
        ("floors", &v.checks.floors),
        ("sink integrity", &v.checks.sink_integrity),
        ("canary", &v.checks.canary),
        ("mismatches", &v.checks.mismatches),
    ] {
        let status = match check.status {
            CheckStatus::Pass => "PASS",
            CheckStatus::Fail => "FAIL",
            CheckStatus::Skipped => "SKIP",
        };
        let _ = writeln!(out, " {name:<15} {status:<4}  {}", check.detail);
    }
    if !v.floors.is_empty() {
        let _ = writeln!(out, "{THIN_RULE}");
        let _ = writeln!(out, " comparisons by floored route (floor in parentheses):");
        for f in &v.floors {
            let mark = if f.met { "OK " } else { "!! " };
            let _ = writeln!(
                out,
                "   {mark}{:<28} {} ({})",
                f.route_id, f.comparisons, f.floor
            );
        }
    }
    if !v.informational.is_empty() {
        let _ = writeln!(out, " skip/failure counters (inspected, not gating):");
        for i in &v.informational {
            let _ = writeln!(
                out,
                "   -  {:<34} route={} reason={} {}",
                i.metric, i.route, i.reason, i.value
            );
        }
    }
    let _ = writeln!(out, "{THIN_RULE}");
    let _ = writeln!(out, " exit {} — {}", v.exit_code, v.verdict);
    let _ = writeln!(out, "{RULE}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::sink::RouteReport;

    fn config(yaml: &str) -> Config {
        serde_yaml::from_str(yaml).expect("test config")
    }

    /// A config with two compared routes (default floor 1) and one exempt.
    fn two_route_config() -> Config {
        config(
            r#"
routes:
  - id: a
    match: { methods: ["GET"], path_prefix: "/a" }
    legacy_upstream: "http://l"
    new_upstream: "http://n"
    mode: shadow_legacy_primary
    comparison: { enabled: true, sample_rate: 1.0 }
  - id: b
    match: { methods: ["GET"], path_prefix: "/b" }
    legacy_upstream: "http://l"
    new_upstream: "http://n"
    mode: shadow_legacy_primary
    comparison: { enabled: true, sample_rate: 1.0, min_comparisons: 0 }
"#,
        )
    }

    fn scrape(text: &str) -> Scrape {
        Scrape::parse(text).expect("test scrape")
    }

    /// A quiescent scrape: route `a` compared 3 times, 1 mismatch, all sunk.
    fn balanced_scrape() -> Scrape {
        scrape(
            r#"
limen_shadow_in_flight 0
limen_diff_sink_enqueued_total 1
limen_diff_sink_written_total 1
limen_diff_sink_dropped_total{reason="queue_full"} 0
limen_diff_sink_dropped_total{reason="io_error"} 0
limen_diff_sink_dropped_total{reason="writer_gone"} 0
limen_comparisons_total{route="a",result="match"} 2
limen_comparisons_total{route="a",result="mismatch"} 1
"#,
        )
    }

    fn report_with(routes: Vec<(&str, usize)>) -> Report {
        let total = routes.iter().map(|(_, n)| n).sum();
        Report {
            routes: routes
                .into_iter()
                .map(|(id, count)| RouteReport {
                    route_id: id.to_string(),
                    count,
                    kinds: Default::default(),
                    examples: Vec::new(),
                })
                .collect(),
            total,
            malformed_lines: 0,
            files_read: 1,
        }
    }

    // -- parser --

    #[test]
    fn parses_labelled_and_bare_samples_with_escapes() {
        let s = scrape("# HELP x\nfoo 1\nbar{route=\"a b\",result=\"mis\\\"match\\\\\"} 2.5\n");
        assert_eq!(s.sum("foo", &[]), Some(1.0));
        assert_eq!(s.sum("bar", &[("route", "a b")]), Some(2.5));
        assert_eq!(s.sum("bar", &[("result", "mis\"match\\")]), Some(2.5));
        assert_eq!(s.sum("absent", &[]), None);
    }

    #[test]
    fn an_unparseable_line_is_an_error_not_a_skip() {
        assert!(Scrape::parse("not a metric line at all{").is_err());
    }

    // -- derivations --

    #[test]
    fn wildcard_hosts_map_to_loopback() {
        assert_eq!(
            control_base_from_listen_addr("0.0.0.0:9090"),
            "http://127.0.0.1:9090"
        );
        assert_eq!(
            control_base_from_listen_addr("[::]:9300"),
            "http://127.0.0.1:9300"
        );
        assert_eq!(
            control_base_from_listen_addr("127.0.0.1:9300"),
            "http://127.0.0.1:9300"
        );
        assert_eq!(
            control_base_from_listen_addr("10.1.2.3:9300"),
            "http://10.1.2.3:9300"
        );
    }

    #[test]
    fn drain_deadline_is_max_shadow_timeout_plus_slack() {
        let cfg = two_route_config();
        // Both routes carry the default 2000ms shadow timeout.
        assert_eq!(
            drain_deadline(&cfg, Duration::from_millis(500)),
            Duration::from_millis(2500)
        );
    }

    // -- evaluate: the decision surface --

    #[test]
    fn clean_run_is_exit_zero() {
        let v = evaluate(
            &two_route_config(),
            &scrape(
                r#"
limen_shadow_in_flight 0
limen_diff_sink_enqueued_total 0
limen_diff_sink_written_total 0
limen_diff_sink_dropped_total{reason="queue_full"} 0
limen_comparisons_total{route="a",result="match"} 3
"#,
            ),
            &report_with(vec![]),
            false,
            DrainStatus::Drained,
        );
        assert_eq!(v.exit_code, 0, "{v:?}");
        assert_eq!(v.verdict, "clean");
    }

    #[test]
    fn mismatches_are_exit_10() {
        let v = evaluate(
            &two_route_config(),
            &balanced_scrape(),
            &report_with(vec![("a", 1)]),
            false,
            DrainStatus::Drained,
        );
        assert_eq!(v.exit_code, 10, "{v:?}");
        assert_eq!(v.mismatches_total, 1);
    }

    #[test]
    fn a_starved_floored_route_is_exit_20() {
        // Route `a` (floor 1) never compared; `b` is explicitly exempt.
        let v = evaluate(
            &two_route_config(),
            &scrape(
                r#"
limen_shadow_in_flight 0
limen_diff_sink_enqueued_total 0
limen_diff_sink_written_total 0
limen_diff_sink_dropped_total{reason="io_error"} 0
"#,
            ),
            &report_with(vec![]),
            false,
            DrainStatus::Drained,
        );
        assert_eq!(v.exit_code, 20, "{v:?}");
        assert!(v.checks.floors.detail.contains('a'));
        assert_eq!(v.floors.len(), 1, "exempt route b is not floored");
    }

    #[test]
    fn a_config_that_floors_nothing_fails_floors() {
        let cfg = config(
            r#"
routes:
  - id: only
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "http://l"
    new_upstream: "http://n"
    mode: shadow_legacy_primary
    comparison: { enabled: false }
"#,
        );
        let v = evaluate(
            &cfg,
            &balanced_scrape(),
            &report_with(vec![]),
            false,
            DrainStatus::Drained,
        );
        assert_eq!(v.checks.floors.status, CheckStatus::Fail);
        assert!(v.checks.floors.detail.contains("compares nothing"));
    }

    #[test]
    fn sink_drops_are_exit_30() {
        let v = evaluate(
            &two_route_config(),
            &scrape(
                r#"
limen_shadow_in_flight 0
limen_diff_sink_enqueued_total 2
limen_diff_sink_written_total 1
limen_diff_sink_dropped_total{reason="queue_full"} 1
limen_comparisons_total{route="a",result="mismatch"} 2
"#,
            ),
            // Counter says 2 mismatches; only 1 reached the sink, 1 dropped.
            &report_with(vec![("a", 1)]),
            false,
            DrainStatus::Drained,
        );
        assert_eq!(v.exit_code, 30, "{v:?}");
        assert!(v.checks.sink_integrity.detail.contains("dropped"));
    }

    #[test]
    fn malformed_sink_lines_are_exit_30() {
        let mut report = report_with(vec![]);
        report.malformed_lines = 1;
        let v = evaluate(
            &two_route_config(),
            &balanced_scrape(),
            &report,
            false,
            DrainStatus::Drained,
        );
        // The balanced scrape carries one mismatch the report lost, so both
        // the malformed-line and per-route problems fire; either way: 30.
        assert_eq!(v.exit_code, 30, "{v:?}");
        assert!(v.checks.sink_integrity.detail.contains("unparseable"));
    }

    #[test]
    fn an_unknown_counter_route_is_exit_30() {
        let v = evaluate(
            &two_route_config(),
            &scrape(
                r#"
limen_shadow_in_flight 0
limen_diff_sink_enqueued_total 0
limen_diff_sink_written_total 0
limen_diff_sink_dropped_total{reason="io_error"} 0
limen_comparisons_total{route="a",result="match"} 1
limen_comparisons_total{route="ghost",result="match"} 1
"#,
            ),
            &report_with(vec![]),
            false,
            DrainStatus::Drained,
        );
        assert_eq!(v.exit_code, 30, "{v:?}");
        assert!(v.checks.sink_integrity.detail.contains("ghost"));
    }

    #[test]
    fn per_route_disagreement_is_exit_30_even_when_totals_compensate() {
        // Route a: counter 1, sink 0; phantom sink route c: counter absent,
        // sink 1. Totals agree (1 == 1); per-route must still fail.
        let v = evaluate(
            &two_route_config(),
            &scrape(
                r#"
limen_shadow_in_flight 0
limen_diff_sink_enqueued_total 1
limen_diff_sink_written_total 1
limen_diff_sink_dropped_total{reason="io_error"} 0
limen_comparisons_total{route="a",result="mismatch"} 1
"#,
            ),
            &report_with(vec![("b", 1)]),
            false,
            DrainStatus::Drained,
        );
        assert_eq!(v.exit_code, 30, "{v:?}");
    }

    #[test]
    fn drain_timeout_dominates_everything() {
        let v = evaluate(
            &two_route_config(),
            &balanced_scrape(),
            &report_with(vec![("a", 1)]),
            false,
            DrainStatus::TimedOut,
        );
        assert_eq!(v.exit_code, 40, "{v:?}");
        assert_eq!(v.verdict, "drain-timeout");
    }

    #[test]
    fn over_balance_is_integrity_not_timeout() {
        let v = evaluate(
            &two_route_config(),
            &balanced_scrape(),
            &report_with(vec![("a", 1)]),
            false,
            DrainStatus::OverBalanced,
        );
        assert_eq!(v.exit_code, 30, "{v:?}");
    }

    // -- canary --

    #[test]
    fn canary_requires_equal_counters_and_at_least_one_record() {
        let s = scrape(
            r#"
limen_shadow_in_flight 0
limen_diff_sink_enqueued_total 1
limen_diff_sink_written_total 1
limen_diff_sink_dropped_total{reason="io_error"} 0
limen_comparisons_total{route="a",result="match"} 2
limen_comparisons_total{route="__limen_canary__",result="mismatch"} 1
"#,
        );
        let ok = evaluate(
            &two_route_config(),
            &s,
            &report_with(vec![("__limen_canary__", 1)]),
            true,
            DrainStatus::Drained,
        );
        assert_eq!(ok.exit_code, 0, "{ok:?}");
        assert_eq!(ok.canary_records, 1);
        assert_eq!(ok.mismatches_total, 0, "canary excluded from the total");

        // Counter says 1, sink has none: the pipeline did not demonstrably
        // bite. (Also a per-route integrity divergence — both fire, exit 30.)
        let missing = evaluate(
            &two_route_config(),
            &s,
            &report_with(vec![]),
            true,
            DrainStatus::Drained,
        );
        assert_eq!(missing.exit_code, 30, "{missing:?}");
        assert_eq!(missing.checks.canary.status, CheckStatus::Fail);
    }

    #[test]
    fn requested_canary_with_no_evidence_at_all_fails() {
        // Neither counters nor sink saw a canary: >= 1 is violated.
        let v = evaluate(
            &two_route_config(),
            &scrape(
                r#"
limen_shadow_in_flight 0
limen_diff_sink_enqueued_total 0
limen_diff_sink_written_total 0
limen_diff_sink_dropped_total{reason="io_error"} 0
limen_comparisons_total{route="a",result="match"} 1
"#,
            ),
            &report_with(vec![]),
            true,
            DrainStatus::Drained,
        );
        assert_eq!(v.checks.canary.status, CheckStatus::Fail);
        assert_eq!(v.exit_code, 30, "{v:?}");
    }

    #[test]
    fn stray_canary_records_without_the_flag_are_excluded_and_reported() {
        let v = evaluate(
            &two_route_config(),
            &scrape(
                r#"
limen_shadow_in_flight 0
limen_diff_sink_enqueued_total 1
limen_diff_sink_written_total 1
limen_diff_sink_dropped_total{reason="io_error"} 0
limen_comparisons_total{route="__limen_canary__",result="mismatch"} 1
limen_comparisons_total{route="a",result="match"} 1
"#,
            ),
            &report_with(vec![("__limen_canary__", 1)]),
            false,
            DrainStatus::Drained,
        );
        assert_eq!(v.exit_code, 0, "{v:?}");
        assert_eq!(v.canary_records, 1);
        assert_eq!(v.mismatches_total, 0);
        assert!(v.checks.canary.detail.contains("excluded"));
    }

    // -- offline --

    #[test]
    fn offline_skips_live_checks_and_restricts_codes() {
        let clean = evaluate(
            &two_route_config(),
            &Scrape::default(),
            &report_with(vec![]),
            false,
            DrainStatus::Offline,
        );
        assert_eq!(clean.exit_code, 0);
        assert_eq!(clean.mode, "offline");
        assert_eq!(clean.checks.drain.status, CheckStatus::Skipped);
        assert_eq!(clean.checks.floors.status, CheckStatus::Skipped);
        assert_eq!(clean.checks.sink_integrity.status, CheckStatus::Skipped);
        assert_eq!(clean.checks.canary.status, CheckStatus::Skipped);

        let dirty = evaluate(
            &two_route_config(),
            &Scrape::default(),
            &report_with(vec![("a", 2)]),
            false,
            DrainStatus::Offline,
        );
        assert_eq!(dirty.exit_code, 10);
    }

    // -- precedence --

    #[test]
    fn highest_code_wins_when_several_conditions_hold() {
        // Mismatches (10) + starved floor (20) + drops (30): 30 wins.
        let cfg = config(
            r#"
routes:
  - id: a
    match: { methods: ["GET"], path_prefix: "/a" }
    legacy_upstream: "http://l"
    new_upstream: "http://n"
    mode: shadow_legacy_primary
    comparison: { enabled: true, sample_rate: 1.0 }
  - id: starved
    match: { methods: ["GET"], path_prefix: "/s" }
    legacy_upstream: "http://l"
    new_upstream: "http://n"
    mode: shadow_legacy_primary
    comparison: { enabled: true, sample_rate: 1.0 }
"#,
        );
        let v = evaluate(
            &cfg,
            &scrape(
                r#"
limen_shadow_in_flight 0
limen_diff_sink_enqueued_total 2
limen_diff_sink_written_total 1
limen_diff_sink_dropped_total{reason="queue_full"} 1
limen_comparisons_total{route="a",result="mismatch"} 2
"#,
            ),
            &report_with(vec![("a", 1)]),
            false,
            DrainStatus::Drained,
        );
        assert_eq!(v.exit_code, 30, "{v:?}");
        assert_eq!(v.checks.mismatches.status, CheckStatus::Fail);
        assert_eq!(v.checks.floors.status, CheckStatus::Fail);
        assert_eq!(v.checks.sink_integrity.status, CheckStatus::Fail);
    }

    // -- rendering --

    #[test]
    fn human_render_names_the_exit_and_marks_offline_degraded() {
        let v = evaluate(
            &two_route_config(),
            &Scrape::default(),
            &report_with(vec![]),
            false,
            DrainStatus::Offline,
        );
        let text = render_human(&v);
        assert!(text.contains("LIMEN VERDICT (offline)"));
        assert!(text.contains("DEGRADED"));
        assert!(text.contains("exit 0 — clean"));
    }

    #[test]
    fn json_output_carries_every_check() {
        let v = evaluate(
            &two_route_config(),
            &balanced_scrape(),
            &report_with(vec![("a", 1)]),
            false,
            DrainStatus::Drained,
        );
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
        assert_eq!(json["exit_code"], 10);
        assert_eq!(json["verdict"], "mismatches-found");
        for check in ["drain", "floors", "sink_integrity", "canary", "mismatches"] {
            assert!(json["checks"][check]["status"].is_string(), "{check}");
        }
        assert_eq!(json["mismatches_total"], 1);
    }
}
