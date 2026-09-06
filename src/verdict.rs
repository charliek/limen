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
    expected_skip_series, COMPARISONS_TOTAL, COMPARISON_SKIPPED_TOTAL, DIFF_SINK_DROPPED_TOTAL,
    DIFF_SINK_ENQUEUED_TOTAL, DIFF_SINK_WRITTEN_TOTAL, SHADOW_FAILED_TOTAL, SHADOW_IN_FLIGHT,
    SHADOW_SKIPPED_TOTAL,
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
const WATCHED_SERIES: [&str; 8] = [
    SHADOW_IN_FLIGHT,
    DIFF_SINK_ENQUEUED_TOTAL,
    DIFF_SINK_WRITTEN_TOTAL,
    DIFF_SINK_DROPPED_TOTAL,
    COMPARISONS_TOTAL,
    // The three gating families: a skip landing between two scrapes is exactly
    // the mid-flight state the drain loop must not conclude over, and their
    // counts now decide a floor, so they get the same exact-integer validation
    // as the counts they sit beside.
    SHADOW_SKIPPED_TOTAL,
    COMPARISON_SKIPPED_TOTAL,
    SHADOW_FAILED_TOTAL,
];

/// The families whose `{route, reason}` labels a floored route's standing is
/// computed from: sampled work that was *not* compared.
///
/// Public so `limen report --format html` requires them the same way
/// [`validate_scrape`] does — per configured route, not per family — rather
/// than keeping a second copy of the list that could drift from this one.
pub const UNCOMPARED_SERIES: [&str; 3] = [
    SHADOW_SKIPPED_TOTAL,
    COMPARISON_SKIPPED_TOTAL,
    SHADOW_FAILED_TOTAL,
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
    /// A floored route did not produce trustworthy evidence: it compared fewer
    /// times than its floor (*starved*), or it met its floor but some of its
    /// sampled work went uncompared — skipped or failed on the shadow leg
    /// (*undermined*) — or the config floors nothing at all.
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
    /// The value exactly as it appeared on the wire.
    ///
    /// `value` is an `f64` because the drain loop's arithmetic is, but an
    /// `f64` cannot represent every counter a scrape can carry: `2^64` reads
    /// back as a finite, integral, non-negative float and saturates to
    /// `u64::MAX` on cast. Readers that need an exact count parse this token
    /// instead, so a value they cannot represent is a refusal rather than a
    /// fabrication. (This verdict's own math keeps using `value`; it validates
    /// the exact-integer range it needs separately.)
    pub raw_value: String,
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

    /// All samples of a metric family. Public so the HTML report can render a
    /// family's rows off the same parser this verdict does its math with,
    /// rather than growing a second exposition reader that could drift from it.
    pub fn family<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Sample> + 'a {
        self.samples.iter().filter(move |s| s.name == name)
    }

    /// Whether the family has at least one sample (zero-registered counts).
    pub fn has_family(&self, name: &str) -> bool {
        self.family(name).next().is_some()
    }

    /// Whether one *series* — a family plus an exact label subset — has at
    /// least one sample.
    ///
    /// Deliberately not [`Scrape::sum`] with the same labels. `sum` reports
    /// absence for the whole *family*: it sets its "anything here?" flag while
    /// iterating, before the label filter, so a route with no sample under a
    /// family some *other* route populated sums to `Some(0.0)` — a zero
    /// indistinguishable from a registered one. That is exactly the hole a
    /// per-route gate falls into, so presence is proved here first and `sum`'s
    /// semantics are left untouched for the callers that depend on them.
    pub fn has_series(&self, name: &str, labels: &[(&str, &str)]) -> bool {
        self.family(name).any(|s| {
            labels
                .iter()
                .all(|(k, v)| s.labels.get(*k).map(String::as_str) == Some(*v))
        })
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
    let (head, raw_value) = line.rsplit_once(' ')?;
    let raw_value = raw_value.trim();
    let value: f64 = raw_value.parse().ok()?;
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
        raw_value: raw_value.to_string(),
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

/// Sampled work on a floored route that never became a comparison, keyed by
/// the counter family that recorded it and its reason.
#[derive(Debug, Clone, Serialize)]
pub struct Uncompared {
    pub metric: String,
    pub reason: String,
    pub count: u64,
}

/// One floored route's standing: the arithmetic claim (`floor_met`) and the
/// evidence claim (`met`), which are not the same question.
///
/// A skip is always *sampled-then-not-compared* work — every skip site sits
/// downstream of the sampling gate — so a route can sit at its floor while a
/// slice of the traffic it was asked to vouch for went unexamined. `met` stays
/// the "this route's evidence is good" flag, which is what an older HTML reader
/// that knows only `met` reads: it fails closed on an undermined route rather
/// than rendering it clean.
#[derive(Debug, Clone, Serialize)]
pub struct RouteFloor {
    pub route_id: String,
    pub comparisons: u64,
    pub floor: u64,
    /// `comparisons >= floor` — the count alone.
    pub floor_met: bool,
    /// `limen_shadow_skipped_total` + `limen_comparison_skipped_total` on this
    /// route, across every reason.
    pub skipped: u64,
    /// `limen_shadow_failed_total` on this route (timeout + error) — a finding
    /// about the new upstream, not about this tool.
    pub shadow_failures: u64,
    /// The per-reason breakdown behind those two totals.
    pub uncompared: Vec<Uncompared>,
    /// `floor_met && skipped == 0 && shadow_failures == 0`.
    pub met: bool,
}

/// A non-gating counter surfaced for inspection: shadows and comparisons that
/// were skipped or failed on a route with **no floor** (`min_comparisons: 0`),
/// or on a route the config does not declare. A floored route's counts are not
/// here — they are in its [`RouteFloor`], where they gate — so the same number
/// is never printed twice under two contradictory framings.
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
    // The whole reserved namespace is subtracted (not just the canary), so
    // this total and `sink_mismatches_by_route` always agree on what counts
    // as a real mismatch. A non-canary reserved record still fails the
    // per-route integrity reconciliation (no counter will match it) — it is
    // excluded from the *mismatch* answer, not from scrutiny.
    let reserved_records = report
        .routes
        .iter()
        .filter(|r| r.route_id.starts_with(RESERVED_ROUTE_ID_PREFIX))
        .map(|r| r.count as u64)
        .sum::<u64>();
    let mismatches_total = report.total as u64 - reserved_records;
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
        informational: collect_informational(config, scrape),
    };
    let code = verdict.code(drain);
    verdict.verdict = code.name();
    verdict.exit_code = code.exit_code();
    verdict
}

/// The set of route ids this config floors — comparison-enabled with a
/// non-zero effective floor. The one definition of "floored", shared by the
/// floors check and by [`collect_informational`], which partitions on it.
fn floored_route_ids(config: &Config) -> BTreeSet<&str> {
    config
        .routes
        .iter()
        .filter(|r| r.comparison.enabled && r.comparison.effective_min_comparisons() > 0)
        .map(|r| r.id.as_str())
        .collect()
}

/// What an operator should change, keyed on the reason the work went
/// uncompared. Every line names a knob (or, for a shadow failure, says the
/// finding is about `new` and not about limen), because "N skipped" without a
/// remedy is a dead end at exactly the moment a campaign is blocked.
fn remedy_for(metric: &str, reason: &str, count: u64) -> String {
    if metric == SHADOW_FAILED_TOTAL {
        return format!(
            "the new upstream failed to answer {count} shadow(s) ({reason}) — a finding \
             about `new`, not tooling noise; read its logs before re-running"
        );
    }
    match reason {
        "response_too_large" => "raise `comparison.max_body_bytes` on this route (default \
             262144); the largest bodies are the ones that went uncompared"
            .to_string(),
        "request_too_large" => "only on routes with `shadow_methods`: the opted-in write's \
             request body exceeded `comparison.max_body_bytes`; raise it or drop the opt-in"
            .to_string(),
        "concurrency_limit" => "`server.shadow_concurrency_limit` is GLOBAL; a slow-on-`new` \
             route holds a slot until its shadow completes or `shadow_ms` expires, so the \
             route that consumed the slots may be a different one — raise the cap or lower \
             drive concurrency"
            .to_string(),
        "response_buffer_timeout" => "the primary answered too slowly to buffer within \
             `timeouts.primary_ms`; raise the budget or lower the load"
            .to_string(),
        // Unsatisfiable by construction: every sampled request on this route
        // skips on content type before a byte is buffered, so no amount of
        // driving can move the comparison count off zero while it is floored.
        "event_stream" => "this route streams, so it can never meet a floor while it has \
             one — every sampled request is skipped on content type before a byte is \
             buffered; unfloor it (`min_comparisons: 0`) and relay it (`legacy_only`)"
            .to_string(),
        other => format!("uncompared sampled work on this route ({other})"),
    }
}

/// The floors check: every enabled route with a non-zero floor must have
/// recorded at least that many comparisons **and** have left no sampled work
/// uncompared; a config that floors nothing fails outright (a verdict over it
/// would prove nothing — and, now that skips gate, unflooring is the tempting
/// way to silence this check).
fn evaluate_floors(config: &Config, scrape: &Scrape) -> (Check, Vec<RouteFloor>) {
    let floored = floored_route_ids(config);
    let floored: Vec<&crate::config::model::RouteConfig> = config
        .routes
        .iter()
        .filter(|r| floored.contains(r.id.as_str()))
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
    let mut undermined = Vec::new();
    let mut remedies = Vec::new();
    for route in floored {
        // Absent-from-scrape deliberately reads as 0 here: with a floor >= 1
        // that is fail-closed (the whole point of the floor). The uncompared
        // sums below are a different matter — a zero there is *permissive*, so
        // it has to be earned: `validate_scrape` has already required this
        // exact route's series under each of the three families, one series at
        // a time, so their zero is a fact rather than an assumption. Nothing
        // weaker would do: `sum` answers `Some(0.0)` for a route with no
        // sample as soon as any other route has one.
        let comparisons = scrape
            .sum(COMPARISONS_TOTAL, &[("route", &route.id)])
            .unwrap_or(0.0) as u64;
        let floor = route.comparison.effective_min_comparisons();
        let floor_met = comparisons >= floor;

        // `sum` matches a label subset, so this adds every reason (and every
        // duplicate sample) the family carries for this route.
        let skipped = [SHADOW_SKIPPED_TOTAL, COMPARISON_SKIPPED_TOTAL]
            .iter()
            .map(|m| scrape.sum(m, &[("route", &route.id)]).unwrap_or(0.0) as u64)
            .sum::<u64>();
        let shadow_failures = scrape
            .sum(SHADOW_FAILED_TOTAL, &[("route", &route.id)])
            .unwrap_or(0.0) as u64;
        let uncompared = uncompared_rows(scrape, &route.id);
        let met = floor_met && skipped == 0 && shadow_failures == 0;

        if !floor_met {
            starved.push(route.id.clone());
        } else if !met {
            undermined.push(route.id.clone());
        }
        // Emitted for any route carrying uncompared work, starved or not: a
        // route can be both, and the operator needs the knob either way.
        for row in &uncompared {
            remedies.push(format!(
                "   {} [{}={} ×{}]: {}",
                route.id,
                row.metric,
                row.reason,
                row.count,
                remedy_for(&row.metric, &row.reason, row.count)
            ));
        }

        rows.push(RouteFloor {
            route_id: route.id.clone(),
            comparisons,
            floor,
            floor_met,
            skipped,
            shadow_failures,
            uncompared,
            met,
        });
    }
    let check = if rows.iter().all(|r| r.met) {
        Check::pass(format!(
            "{} floored route(s) all at/above floor, with no uncompared sampled work",
            rows.len()
        ))
    } else {
        let mut problems = Vec::new();
        if !starved.is_empty() {
            problems.push(format!(
                "starved — below their comparison floor: {} — a route that never compared \
                 cannot contribute a mismatch, so a clean total proves nothing about it",
                starved.join(", ")
            ));
        }
        if !undermined.is_empty() {
            problems.push(format!(
                "undermined — at their floor but with sampled work that was never compared: \
                 {} — the count overstates what was actually verified",
                undermined.join(", ")
            ));
        }
        let mut detail = problems.join("; ");
        for remedy in &remedies {
            detail.push('\n');
            detail.push_str(remedy);
        }
        Check::fail(detail)
    };
    (check, rows)
}

/// One route's uncompared work, per `{metric, reason}`, zeros omitted.
/// Duplicate samples of the same pair are summed rather than listed twice —
/// the exposition may legally repeat a series, and two rows saying `×1` would
/// misrepresent one count as two findings.
fn uncompared_rows(scrape: &Scrape, route_id: &str) -> Vec<Uncompared> {
    let mut totals: BTreeMap<(&str, String), u64> = BTreeMap::new();
    for metric in UNCOMPARED_SERIES {
        for sample in scrape.family(metric) {
            if sample.labels.get("route").map(String::as_str) != Some(route_id) {
                continue;
            }
            let reason = sample.labels.get("reason").cloned().unwrap_or_default();
            *totals.entry((metric, reason)).or_default() += sample.value as u64;
        }
    }
    totals
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|((metric, reason), count)| Uncompared {
            metric: metric.to_string(),
            reason,
            count,
        })
        .collect()
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
    // proxy started, or a scrape aimed at a different limen instance. The
    // uncompared families are read with the same suspicion as the comparison
    // counter now that a floor turns on them — a skip attributed to a route
    // this config does not declare is the same drift, told by a different
    // series, and reading it as "not my route, therefore harmless" would let a
    // wrong-instance scrape pass the very check it should trip.
    let known: BTreeSet<&str> = config.routes.iter().map(|r| r.id.as_str()).collect();
    let mut unknown: BTreeSet<String> = BTreeSet::new();
    // `limen_comparisons_total` keeps the reserved-namespace exemption: the
    // sink canary is a real, legitimate producer there, and every campaign
    // scrape carries its row.
    unknown.extend(
        scrape
            .label_values(COMPARISONS_TOTAL, "route")
            .into_iter()
            .filter(|r| !r.starts_with(RESERVED_ROUTE_ID_PREFIX) && !known.contains(r.as_str())),
    );
    // The uncompared families get **no** such exemption. The reserved
    // namespace exists for limen's own canary, which rides the compare → sink
    // path and produces sink records and comparison counters — it never skips
    // and never fails a shadow, so under these three families there is nothing
    // legitimate to exempt. Exempting it anyway would leave one namespace in
    // which a whole route's uncompared work is invisible to this check while
    // the config has never heard of it, which is the drift the check is for.
    for metric in UNCOMPARED_SERIES {
        unknown.extend(
            scrape
                .label_values(metric, "route")
                .into_iter()
                .filter(|r| !known.contains(r.as_str())),
        );
    }
    let unknown: Vec<String> = unknown.into_iter().collect();
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
/// [`InfoCounter`]) — the unfloored routes only, plus any route the config
/// does not declare (which the integrity check is failing anyway, and which
/// must not vanish from the output on its way there).
///
/// Filtered on the **config's** floored set rather than on the `floors` rows:
/// `--offline` produces no floor rows at all, and partitioning on those would
/// silently widen the offline listing to every route while claiming to be the
/// same rule.
fn collect_informational(config: &Config, scrape: &Scrape) -> Vec<InfoCounter> {
    let floored = floored_route_ids(config);
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
            if s.labels
                .get("route")
                .is_some_and(|r| floored.contains(r.as_str()))
            {
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

/// Families whose presence in the scrape is mandatory: their absence means the
/// instrumentation the whole verdict rests on is missing (a proxy older than
/// the verdict tool, or a renderer regression) — never "zero events".
///
/// These seven and no others, because these seven and no others are pre-touched
/// before traffic: the four process-wide pipeline series by
/// [`crate::observability::prometheus::register_verdict_series`], and the three
/// route-labelled uncompared families by
/// [`crate::observability::prometheus::register_skip_series`]. The uncompared
/// families joined the set when they started gating a floor — a check may not
/// rest on a family it cannot tell "absent" from "zero" on — and their absence
/// is now a *version boundary*: a proxy older than this tool exports none of
/// them, and says so as exit 50 rather than as a green run over evidence
/// nobody could see. Every other family limen exports is still registered
/// lazily, on the first event of its kind. Public so `limen report --format
/// html` can mirror this contract rather than restate it (see
/// [`crate::report_html`]).
///
/// This list is the *family*-level contract, which is all a reader holding a
/// scrape and no config can check. [`validate_scrape`] holds the config, so it
/// requires the three uncompared families one series at a time — every
/// configured route × every reason, from
/// [`crate::observability::prometheus::expected_skip_series`] — and skips them
/// here. A family-level check alone would let one route's skips vouch for
/// every other route.
pub const REQUIRED_SERIES: [&str; 7] = [
    SHADOW_IN_FLIGHT,
    DIFF_SINK_ENQUEUED_TOTAL,
    DIFF_SINK_WRITTEN_TOTAL,
    DIFF_SINK_DROPPED_TOTAL,
    SHADOW_SKIPPED_TOTAL,
    COMPARISON_SKIPPED_TOTAL,
    SHADOW_FAILED_TOTAL,
];

/// Everything a scrape must satisfy before any check reads a number off it:
/// the required series are present, every watched count is an exact integer,
/// and every sample under a gating family carries the labels the gate reads.
///
/// Pure and public so the drain loop and a test can hold the same contract:
/// each failure is [`InputUnavailable`] (exit 50), never a downgraded verdict,
/// because a scrape limen cannot read is a fact about the tooling and not about
/// the migration.
///
/// Takes the config because the uncompared families are required **per
/// series**, not per family: see the loop below.
pub fn validate_scrape(config: &Config, scrape: &Scrape) -> Result<(), InputUnavailable> {
    for series in REQUIRED_SERIES {
        // The uncompared families are required route by route just below,
        // which is strictly stronger — and which, unlike a family-level check,
        // demands nothing of a config that declares no routes (where the
        // registrar emits nothing either, so the honest answer is "no
        // instrumentation was owed", not "older binary").
        if UNCOMPARED_SERIES.contains(&series) {
            continue;
        }
        if !scrape.has_family(series) {
            return Err(InputUnavailable(format!(
                "required series {series} absent from the scrape — the proxy is not \
                 exporting the verdict instrumentation (older binary?)"
            )));
        }
    }
    // The route-labelled half of the version boundary, and the reason the
    // floors check may read a route's zero as a fact.
    //
    // A family-level "is this metric here at all?" check does not survive
    // contact with a real campaign: `Scrape::sum` reports absence per *family*
    // (it sets `any` while iterating, before the label filter), so one route
    // that skipped once makes every other route's `sum(.., route=..)` return
    // `Some(0.0)`. Against an older, lazily-registering proxy that recorded a
    // skip somewhere — a long campaign always does — all three families exist,
    // a family-level check passes, and a floored route with no samples at all
    // reads `skipped = 0, shadow_failures = 0` and is declared met. That is the
    // false green this gate exists to kill, wearing the gate's own colours.
    //
    // So require exactly what `register_skip_series` registers, from the same
    // enumeration it registers from: every configured route × every uncompared
    // family × every reason of that family.
    for (route_id, family, reason) in
        expected_skip_series(config.routes.iter().map(|r| r.id.as_str()))
    {
        if !scrape.has_series(family, &[("route", route_id), ("reason", reason)]) {
            return Err(InputUnavailable(format!(
                "required series {family}{{route=\"{route_id}\",reason=\"{reason}\"}} \
                 absent from the scrape — this proxy predates the gate that reads it \
                 (older binary?): it registers this family lazily, so a route it never \
                 skipped on says nothing where a current limen says zero, and nothing \
                 cannot be gated on"
            )));
        }
    }
    // Every count this verdict compares must be an exact integer: past 2^53 an
    // f64 `==` can equate values that differ by one, which is precisely the
    // discrepancy the integrity checks exist to catch (adversarial review).
    // Validated once here so every downstream comparison and cast works on
    // exact values.
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
    // A gating family's labels are limen's own exposition, not operator input:
    // a sample without a `route` cannot be attributed to a floor, and one
    // without a `reason` cannot be given a remedy. Either means the renderer
    // regressed, so it fails closed rather than being silently attributed to
    // the empty route.
    for name in UNCOMPARED_SERIES {
        for sample in scrape.family(name) {
            for label in ["route", "reason"] {
                if !sample.labels.contains_key(label) {
                    return Err(InputUnavailable(format!(
                        "series {name} carries a sample with no `{label}` label — limen \
                         labels every sample of this family, so this is a renderer \
                         regression, not a quiet zero"
                    )));
                }
            }
        }
    }
    Ok(())
}

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

    let (scrape, drain) = drain(&client, config, opts).await?;
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
    config: &Config,
    opts: &VerdictOptions,
) -> Result<(Scrape, DrainStatus), InputUnavailable> {
    let deadline = Instant::now() + opts.drain_deadline;
    let mut prev: Option<BTreeMap<(String, String), f64>> = None;
    loop {
        let scrape = fetch_scrape(client, opts).await?;
        validate_scrape(config, &scrape)?;
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
        // A detail may carry remedy lines (the floors check does); they are
        // written as their own lines rather than run together, so the operator
        // reads route, reason and knob without opening the JSON.
        let mut lines = check.detail.split('\n');
        let head = lines.next().unwrap_or_default();
        let _ = writeln!(out, " {name:<15} {status:<4}  {head}");
        for line in lines {
            let _ = writeln!(out, "{line}");
        }
    }
    if !v.floors.is_empty() {
        let _ = writeln!(out, "{THIN_RULE}");
        let _ = writeln!(
            out,
            " comparisons by floored route (floor in parentheses, then work never compared):"
        );
        for f in &v.floors {
            let mark = if f.met { "OK " } else { "!! " };
            let uncompared = f.skipped + f.shadow_failures;
            let _ = writeln!(
                out,
                "   {mark}{:<28} {} ({}) uncompared={}",
                f.route_id, f.comparisons, f.floor, uncompared
            );
        }
    }
    if !v.informational.is_empty() {
        let _ = writeln!(
            out,
            " skip/failure counters (inspected, not gating — routes with no floor):"
        );
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
    use crate::observability::{ShadowFailure, SkipReason};

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
    // -- uncompared sampled work: the gate (limen#23, limen#24) --

    /// A drained, balanced scrape for route `a` sitting **five comparisons
    /// above** its floor of 1, plus whatever the case under test adds.
    ///
    /// Every fixture in this section starts here on purpose: a route below its
    /// floor already fails through the *starvation* branch, so a case built on
    /// a starved route would pass whether or not skips gate at all — the
    /// mutation these tests exist to kill would survive it.
    fn at_floor_with(extra: &str) -> Scrape {
        scrape(&format!(
            r#"
limen_shadow_in_flight 0
limen_diff_sink_enqueued_total 0
limen_diff_sink_written_total 0
limen_diff_sink_dropped_total{{reason="io_error"}} 0
limen_comparisons_total{{route="a",result="match"}} 5
{extra}
"#
        ))
    }

    /// `evaluate` over [`at_floor_with`], with the default two-route config.
    fn verdict_at_floor_with(extra: &str) -> VerdictReport {
        evaluate(
            &two_route_config(),
            &at_floor_with(extra),
            &report_with(vec![]),
            false,
            DrainStatus::Drained,
        )
    }

    /// The knob each reason's remedy line must name, so an operator reads what
    /// to change out of the verdict itself rather than out of the source.
    fn knob_for(reason: SkipReason) -> &'static str {
        match reason {
            SkipReason::ConcurrencyLimit => "server.shadow_concurrency_limit",
            SkipReason::ResponseTooLarge | SkipReason::RequestTooLarge => {
                "comparison.max_body_bytes"
            }
            SkipReason::EventStream => "min_comparisons: 0",
            SkipReason::ResponseBufferTimeout => "timeouts.primary_ms",
        }
    }

    #[test]
    fn every_skip_reason_undermines_a_floored_route_that_met_its_floor() {
        for metric in [SHADOW_SKIPPED_TOTAL, COMPARISON_SKIPPED_TOTAL] {
            for reason in SkipReason::ALL {
                let v = verdict_at_floor_with(&format!(
                    r#"{metric}{{route="a",reason="{}"}} 3"#,
                    reason.as_str()
                ));
                let detail = &v.checks.floors.detail;
                assert_eq!(v.exit_code, 20, "{metric}/{}: {v:?}", reason.as_str());
                assert!(
                    detail.contains("undermined — at their floor"),
                    "{metric}/{}: starvation wording would mean the fixture never \
                     reached its floor: {detail}",
                    reason.as_str()
                );
                assert!(detail.contains("a ["), "{detail}");
                assert!(
                    detail.contains(&format!("{metric}={} \u{d7}3", reason.as_str())),
                    "{detail}"
                );
                assert!(detail.contains(knob_for(reason)), "{detail}");

                let row = v.floors.iter().find(|f| f.route_id == "a").unwrap();
                assert!(row.floor_met, "the fixture must sit above its floor");
                assert!(!row.met);
                assert_eq!(row.skipped, 3);
                assert_eq!(row.shadow_failures, 0);
                assert_eq!(row.uncompared.len(), 1);
                assert_eq!(row.uncompared[0].reason, reason.as_str());
                assert_eq!(row.uncompared[0].count, 3);
            }
        }
    }

    #[test]
    fn every_shadow_failure_undermines_a_floored_route_that_met_its_floor() {
        for failure in ShadowFailure::ALL {
            let v = verdict_at_floor_with(&format!(
                r#"{SHADOW_FAILED_TOTAL}{{route="a",reason="{}"}} 7"#,
                failure.as_str()
            ));
            let detail = &v.checks.floors.detail;
            assert_eq!(v.exit_code, 20, "{}: {v:?}", failure.as_str());
            assert!(detail.contains("undermined — at their floor"), "{detail}");
            assert!(
                detail.contains(&format!(
                    "{SHADOW_FAILED_TOTAL}={} \u{d7}7",
                    failure.as_str()
                )),
                "{detail}"
            );
            // The remedy points at `new`, not at a limen knob: a shadow that
            // failed is a finding about the upstream being migrated to.
            assert!(detail.contains("a finding about `new`"), "{detail}");
            assert!(detail.contains("read its logs"), "{detail}");

            let row = v.floors.iter().find(|f| f.route_id == "a").unwrap();
            assert!(row.floor_met);
            assert!(!row.met);
            assert_eq!(row.shadow_failures, 7);
            assert_eq!(row.skipped, 0);
        }
    }

    /// A floored route that streams can never pass while it is floored, and
    /// the remedy says so — otherwise an operator drives a whole campaign to
    /// discover it.
    #[test]
    fn the_event_stream_remedy_says_the_floor_is_unsatisfiable() {
        let v = verdict_at_floor_with(
            r#"limen_comparison_skipped_total{route="a",reason="event_stream"} 1"#,
        );
        let detail = &v.checks.floors.detail;
        assert!(
            detail.contains("can never meet a floor while it has one"),
            "{detail}"
        );
        assert!(detail.contains("legacy_only"), "{detail}");
    }

    #[test]
    fn several_reasons_and_duplicate_samples_sum_into_one_route() {
        let v = verdict_at_floor_with(
            r#"limen_shadow_skipped_total{route="a",reason="concurrency_limit"} 2
limen_shadow_skipped_total{route="a",reason="concurrency_limit"} 3
limen_comparison_skipped_total{route="a",reason="response_too_large"} 4
limen_shadow_failed_total{route="a",reason="timeout"} 6"#,
        );
        assert_eq!(v.exit_code, 20, "{v:?}");
        let row = v.floors.iter().find(|f| f.route_id == "a").unwrap();
        assert_eq!(row.skipped, 9, "2 + 3 duplicates + 4");
        assert_eq!(row.shadow_failures, 6);
        // Two rows, not three: the duplicated series is one count.
        assert_eq!(
            row.uncompared
                .iter()
                .map(|u| (u.metric.as_str(), u.reason.as_str(), u.count))
                .collect::<Vec<_>>(),
            vec![
                (COMPARISON_SKIPPED_TOTAL, "response_too_large", 4),
                (SHADOW_FAILED_TOTAL, "timeout", 6),
                (SHADOW_SKIPPED_TOTAL, "concurrency_limit", 5),
            ]
        );
    }

    /// Per route, not per process: `b`'s skips say nothing about whether `a`'s
    /// evidence is good. (`b` is `min_comparisons: 0` here, so its own skips
    /// are informational — see the next test.)
    #[test]
    fn a_skip_on_another_route_does_not_undermine_a_floored_route() {
        let v = verdict_at_floor_with(
            r#"limen_shadow_skipped_total{route="b",reason="concurrency_limit"} 11
limen_shadow_failed_total{route="b",reason="error"} 4"#,
        );
        assert_eq!(v.exit_code, 0, "{v:?}");
        let row = v.floors.iter().find(|f| f.route_id == "a").unwrap();
        assert!(row.met);
        assert_eq!(row.skipped, 0);
        assert_eq!(row.shadow_failures, 0);
        assert!(row.uncompared.is_empty());
    }

    #[test]
    fn an_unfloored_routes_skips_are_informational_and_do_not_gate() {
        let v = verdict_at_floor_with(
            r#"limen_comparison_skipped_total{route="b",reason="event_stream"} 9"#,
        );
        assert_eq!(v.exit_code, 0, "{v:?}");
        let row = v
            .informational
            .iter()
            .find(|i| i.route == "b")
            .expect("an unfloored route's skips stay visible");
        assert_eq!(row.metric, COMPARISON_SKIPPED_TOTAL);
        assert_eq!(row.reason, "event_stream");
        assert_eq!(row.value, 9);
        let text = render_human(&v);
        assert!(text.contains("routes with no floor"), "{text}");
    }

    /// The same number must never be printed twice under two contradictory
    /// framings: a floored route's skips gate, so they belong to its floor row
    /// and nowhere else.
    #[test]
    fn a_floored_routes_skips_are_not_repeated_as_informational() {
        let v = verdict_at_floor_with(
            r#"limen_comparison_skipped_total{route="a",reason="response_too_large"} 2
limen_comparison_skipped_total{route="b",reason="response_too_large"} 2"#,
        );
        assert_eq!(v.exit_code, 20, "{v:?}");
        assert!(
            v.informational.iter().all(|i| i.route != "a"),
            "a floored route's counts gate in its floor row; listing them as \
             'inspected, not gating' contradicts the exit code: {:?}",
            v.informational
        );
        assert_eq!(v.informational.iter().filter(|i| i.route == "b").count(), 1);
    }

    /// A skip attributed to a route this config never declared is the same
    /// drift an unknown comparisons route is — a config edited after start, or
    /// a scrape of the wrong instance.
    #[test]
    fn a_skip_series_for_an_undeclared_route_is_exit_30() {
        let v =
            verdict_at_floor_with(r#"limen_shadow_failed_total{route="ghost",reason="timeout"} 1"#);
        assert_eq!(v.exit_code, 30, "{v:?}");
        assert!(v.checks.sink_integrity.detail.contains("ghost"), "{v:?}");
    }

    /// The reserved namespace buys no exemption under the uncompared families.
    ///
    /// It exists for limen's own sink canary, which rides compare → sink and
    /// shows up in `limen_comparisons_total` — it never skips and never fails a
    /// shadow, so nothing legitimate produces these three series under a `__`
    /// route id. Exempting the prefix here (as the comparison counter must)
    /// would leave one namespace where a whole route's uncompared work is
    /// merely informational while the config has never heard of it.
    #[test]
    fn a_reserved_namespace_skip_series_for_an_undeclared_route_is_still_exit_30() {
        for line in [
            r#"limen_shadow_skipped_total{route="__ghost",reason="concurrency_limit"} 1"#,
            r#"limen_comparison_skipped_total{route="__ghost",reason="event_stream"} 1"#,
            r#"limen_shadow_failed_total{route="__ghost",reason="error"} 1"#,
            // The canary's own id, which the comparison counter does exempt.
            &format!(
                r#"limen_shadow_skipped_total{{route="{CANARY_ROUTE_ID}",reason="event_stream"}} 1"#
            ),
        ] {
            let v = verdict_at_floor_with(line);
            assert_eq!(v.exit_code, 30, "{line}: {v:?}");
            assert!(
                v.checks
                    .sink_integrity
                    .detail
                    .contains("not in this config"),
                "{line}: {v:?}"
            );
        }
    }

    /// ...while the comparison counter's exemption is untouched: the canary is
    /// a real producer there, and every campaign scrape carries its row.
    #[test]
    fn the_canarys_comparison_counter_is_still_exempt() {
        let v = verdict_at_floor_with(&format!(
            r#"limen_comparisons_total{{route="{CANARY_ROUTE_ID}",result="match"}} 1"#
        ));
        assert_eq!(v.exit_code, 0, "{v:?}");
    }

    /// Unflooring every route is the tempting way to silence this gate, and it
    /// has always been refused: a config that floors nothing proves nothing.
    /// Pinned so the gate cannot quietly acquire an escape hatch.
    #[test]
    fn unflooring_every_route_is_not_an_escape_hatch() {
        let cfg = config(
            r#"
routes:
  - id: a
    match: { methods: ["GET"], path_prefix: "/a" }
    legacy_upstream: "http://l"
    new_upstream: "http://n"
    mode: shadow_legacy_primary
    comparison: { enabled: true, sample_rate: 1.0, min_comparisons: 0 }
  - id: b
    match: { methods: ["GET"], path_prefix: "/b" }
    legacy_upstream: "http://l"
    new_upstream: "http://n"
    mode: shadow_legacy_primary
    comparison: { enabled: true, sample_rate: 1.0, min_comparisons: 0 }
"#,
        );
        let v = evaluate(
            &cfg,
            &at_floor_with(r#"limen_shadow_skipped_total{route="a",reason="concurrency_limit"} 3"#),
            &report_with(vec![]),
            false,
            DrainStatus::Drained,
        );
        assert_eq!(v.exit_code, 20, "{v:?}");
        assert!(v.checks.floors.detail.contains("compares nothing"), "{v:?}");
        assert!(v.floors.is_empty());
    }

    #[test]
    fn floor_met_and_met_stay_consistent_on_a_starved_and_skipped_route() {
        // Route `a` floored at 1, zero comparisons, and a skip on top: starved
        // *and* undermined. It is reported as starved, but it still gets the
        // remedy line — the knob is what unblocks the next drive.
        let v = evaluate(
            &two_route_config(),
            &scrape(
                r#"
limen_shadow_in_flight 0
limen_diff_sink_enqueued_total 0
limen_diff_sink_written_total 0
limen_diff_sink_dropped_total{reason="io_error"} 0
limen_shadow_skipped_total{route="a",reason="concurrency_limit"} 4
"#,
            ),
            &report_with(vec![]),
            false,
            DrainStatus::Drained,
        );
        assert_eq!(v.exit_code, 20, "{v:?}");
        let row = v.floors.iter().find(|f| f.route_id == "a").unwrap();
        assert_eq!(row.comparisons, 0);
        assert!(!row.floor_met);
        assert!(!row.met, "met can never be true where floor_met is false");
        assert_eq!(row.skipped, 4);
        let detail = &v.checks.floors.detail;
        assert!(
            detail.contains("starved — below their comparison floor"),
            "{detail}"
        );
        assert!(
            detail.contains("server.shadow_concurrency_limit"),
            "{detail}"
        );
    }

    #[test]
    fn json_output_carries_the_uncompared_fields() {
        let v = verdict_at_floor_with(
            r#"limen_comparison_skipped_total{route="a",reason="response_too_large"} 2
limen_shadow_failed_total{route="a",reason="error"} 1"#,
        );
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
        assert_eq!(json["exit_code"], 20);
        let row = &json["floors"][0];
        assert_eq!(row["route_id"], "a");
        assert_eq!(row["floor_met"], true);
        assert_eq!(row["met"], false);
        assert_eq!(row["skipped"], 2);
        assert_eq!(row["shadow_failures"], 1);
        assert_eq!(row["uncompared"][0]["metric"], COMPARISON_SKIPPED_TOTAL);
        assert_eq!(row["uncompared"][0]["reason"], "response_too_large");
        assert_eq!(row["uncompared"][0]["count"], 2);
        assert_eq!(row["uncompared"][1]["metric"], SHADOW_FAILED_TOTAL);
    }

    #[test]
    fn the_human_render_shows_the_uncompared_count_and_the_remedy() {
        let v = verdict_at_floor_with(
            r#"limen_comparison_skipped_total{route="a",reason="response_too_large"} 2"#,
        );
        let text = render_human(&v);
        assert!(text.contains("uncompared=2"), "{text}");
        assert!(text.contains("comparison.max_body_bytes"), "{text}");
        assert!(text.contains("exit 20 \u{2014} floors-unmet"), "{text}");
    }

    // -- validate_scrape: what a verdict requires of an exposition --

    /// The four process-wide required families, at zero.
    const PROCESS_WIDE: &str = "\
limen_shadow_in_flight 0
limen_diff_sink_enqueued_total 0
limen_diff_sink_written_total 0
limen_diff_sink_dropped_total{reason=\"io_error\"} 0
";

    /// Every uncompared series a route carries once `register_skip_series` has
    /// run, at zero.
    ///
    /// Written out from the reason enums here rather than from
    /// `expected_skip_series`, so this fixture is a second opinion about what a
    /// live limen renders rather than a restatement of what the validator
    /// asks for. (The two are tied together for real, through the actual
    /// recorder, by `a_scrape_of_exactly_what_the_registrar_renders_validates`.)
    fn registered_for(route_id: &str) -> String {
        let mut out = String::new();
        for reason in SkipReason::ALL {
            for family in [SHADOW_SKIPPED_TOTAL, COMPARISON_SKIPPED_TOTAL] {
                out.push_str(&format!(
                    "{family}{{route=\"{route_id}\",reason=\"{}\"}} 0\n",
                    reason.as_str()
                ));
            }
        }
        for failure in ShadowFailure::ALL {
            out.push_str(&format!(
                "{SHADOW_FAILED_TOTAL}{{route=\"{route_id}\",reason=\"{}\"}} 0\n",
                failure.as_str()
            ));
        }
        out
    }

    /// Exactly what a live limen renders before any traffic, for the two-route
    /// config: every required series present at zero, per route and per reason.
    fn registered() -> String {
        format!(
            "{PROCESS_WIDE}{}{}",
            registered_for("a"),
            registered_for("b")
        )
    }

    /// `validate_scrape`'s refusal as the CLI reports it: exit 50, never a
    /// downgraded verdict.
    fn exit_code_of(text: &str) -> u8 {
        match validate_scrape(&two_route_config(), &scrape(text)) {
            Ok(()) => 0,
            Err(_) => EXIT_INPUT_UNAVAILABLE,
        }
    }

    #[test]
    fn a_fully_registered_scrape_validates() {
        validate_scrape(&two_route_config(), &scrape(&registered())).expect("registered scrape");
    }

    /// The registrar and the validator must want the same series, and the only
    /// way to know is to render one and hand it to the other: a local recorder
    /// runs the real registration, the real Prometheus exporter renders it, and
    /// the real validator reads it. Enumerating the series twice — once to
    /// touch, once to require — is what let a route go unregistered and still
    /// pass; this test fails the moment the two lists part company.
    #[test]
    fn a_scrape_of_exactly_what_the_registrar_renders_validates() {
        use metrics_exporter_prometheus::PrometheusBuilder;

        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::observability::prometheus::register_verdict_series();
            crate::observability::prometheus::register_skip_series(
                two_route_config().routes.iter().map(|r| r.id.as_str()),
            );
        });
        let rendered = handle.render();
        validate_scrape(&two_route_config(), &scrape(&rendered))
            .expect("a scrape of exactly what limen registers must satisfy what limen requires");
    }

    /// The version boundary: a limen older than this gate exports none of the
    /// three uncompared families, and that is a tooling failure rather than
    /// three quiet zeros over work nobody could see.
    #[test]
    fn an_absent_gating_family_is_exit_50() {
        for family in UNCOMPARED_SERIES {
            let full = registered();
            let text: String = full
                .lines()
                .filter(|l| !l.starts_with(family))
                .map(|l| format!("{l}\n"))
                .collect();
            assert_eq!(exit_code_of(&text), EXIT_INPUT_UNAVAILABLE, "{family}");
            let err = validate_scrape(&two_route_config(), &scrape(&text)).unwrap_err();
            assert!(err.0.contains(family), "{err}");
            assert!(err.0.contains("older binary?"), "{err}");
        }
    }

    /// The per-route hole a family-level check leaves open, and the reason this
    /// validator takes a config.
    ///
    /// An older, lazily-registering proxy that skipped *somewhere* exports all
    /// three families — every long campaign produces at least one skip, one
    /// comparison skip and one shadow failure. Under a family-level check the
    /// scrape validates, and floored route `a`, which has no sample in any of
    /// them, sums to `skipped = 0, shadow_failures = 0` and is declared met:
    /// exit 0 over evidence nobody could see. The series must be required for
    /// the route whose floor turns on them.
    #[test]
    fn a_floored_route_with_no_registered_series_is_exit_50_even_when_another_route_has_them() {
        // Route `b` alone carries the families — and carries them non-zero, so
        // the family exists by having been used, exactly as on an old proxy.
        let text = format!(
            "{PROCESS_WIDE}\
limen_shadow_skipped_total{{route=\"b\",reason=\"concurrency_limit\"}} 1
limen_comparison_skipped_total{{route=\"b\",reason=\"response_too_large\"}} 1
limen_shadow_failed_total{{route=\"b\",reason=\"timeout\"}} 1
limen_comparisons_total{{route=\"a\",result=\"match\"}} 5
"
        );
        assert_eq!(exit_code_of(&text), EXIT_INPUT_UNAVAILABLE, "{text}");
        let err = validate_scrape(&two_route_config(), &scrape(&text)).unwrap_err();
        assert!(err.0.contains(SHADOW_SKIPPED_TOTAL), "{err}");
        assert!(err.0.contains(r#"route="a""#), "{err}");
        assert!(err.0.contains("predates the gate"), "{err}");

        // And the mutation this test exists to catch: with the series merely
        // *present* for `b`, `sum` reports zero for `a` rather than absence, so
        // nothing downstream could have noticed.
        let s = scrape(&text);
        assert_eq!(s.sum(SHADOW_SKIPPED_TOTAL, &[("route", "a")]), Some(0.0));
        assert!(!s.has_series(SHADOW_SKIPPED_TOTAL, &[("route", "a")]));
    }

    /// A config with no routes is valid, and a current limen renders none of
    /// the three families for it — there is no route to register one against.
    /// That must not be read as an old binary: the run is already failed, by
    /// the older and more precise "this config compares nothing" arm.
    #[test]
    fn a_config_with_no_routes_is_not_a_version_boundary() {
        let empty = config("routes: []\n");
        assert!(empty.routes.is_empty());
        validate_scrape(&empty, &scrape(PROCESS_WIDE))
            .expect("nothing is registered for a route set that is empty, so nothing is required");
        let v = evaluate(
            &empty,
            &scrape(PROCESS_WIDE),
            &report_with(vec![]),
            false,
            DrainStatus::Drained,
        );
        assert_eq!(v.exit_code, 20, "{v:?}");
        assert!(v.checks.floors.detail.contains("compares nothing"), "{v:?}");
    }

    #[test]
    fn a_gating_sample_missing_route_or_reason_is_exit_50() {
        for line in [
            r#"limen_shadow_skipped_total{reason="concurrency_limit"} 1"#,
            r#"limen_comparison_skipped_total{route="a"} 1"#,
            r#"limen_shadow_failed_total{route="a"} 1"#,
            "limen_shadow_failed_total 1",
        ] {
            let text = format!("{}{line}\n", registered());
            assert_eq!(exit_code_of(&text), EXIT_INPUT_UNAVAILABLE, "{line}");
            let err = validate_scrape(&two_route_config(), &scrape(&text)).unwrap_err();
            assert!(err.0.contains("renderer regression"), "{err}");
        }
    }

    #[test]
    fn a_non_exact_gating_count_is_exit_50() {
        for value in ["1.5", "9007199254740993", "-1"] {
            let text = format!(
                "{}limen_shadow_skipped_total{{route=\"a\",reason=\"event_stream\"}} {value}\n",
                registered()
            );
            assert_eq!(exit_code_of(&text), EXIT_INPUT_UNAVAILABLE, "{value}");
        }
    }
}
