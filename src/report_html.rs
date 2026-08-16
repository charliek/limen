//! `limen report --format html`: a self-contained, fail-closed status page.
//!
//! The page renders artifacts that already exist — a sink directory, a saved
//! `limen verdict --format json` document, a saved `GET /observe/profile`
//! body, a saved `/metrics` scrape, and the config those were produced against.
//! It runs nothing, reaches nothing, and proves nothing on its own.
//!
//! Its one defining property: **it must be unable to render a failure or a
//! missing input as success.** Everything below bends toward that.
//!
//! - **A missing input is never a zero.** An absent artifact is rendered as
//!   "not provided" and, when it is required for a green banner, downgrades the
//!   banner to INCOMPLETE. An artifact that was provided but could not be read
//!   or parsed is a FAILURE — a page that quietly dropped an unreadable verdict
//!   would be reporting on a campaign it never looked at.
//!
//!   The sink directory is the one carve-out, because it is the one input that
//!   is always "provided": an unreadable or absent `--dir` is INCOMPLETE, not
//!   FAILURE, because it is indistinguishable from a campaign that has not
//!   recorded anything yet — the same reason `files_read == 0` is. It is never
//!   CLEAN either way, which is the property that matters.
//! - **An empty sink is not a clean run.** A sink file is created by the first
//!   record written to it, so an empty directory is indistinguishable from a
//!   pipeline that never ran — or one that cannot write at all.
//!   `files_read == 0` is INCOMPLETE; a zero mismatch count across files that
//!   *do* exist is only rendered as clean when a clean verdict whose
//!   `sink_integrity` check passed vouches for it.
//!
//!   This makes CLEAN reachable only once *something* has proven the sink
//!   writes, which is deliberate. In practice that something is the canary:
//!   `limen verdict --canary` rides a record through compare → sink → flush, so
//!   a mismatch-free campaign still leaves a file behind. Canary records are
//!   counted, excluded from the mismatch answer, and reconciled against the
//!   verdict's `canary_records` — exactly as `verdict::evaluate` treats them
//!   (see [`SinkCounts`]). Without the canary, a mismatch-free run has no
//!   evidence to show and the page says so.
//! - **Artifacts are cross-checked, not trusted.** Sink counts are reconciled
//!   against the verdict's per-route map, canary records against its
//!   `canary_records`, verdict floors against the config's
//!   `effective_min_comparisons()`, and every route id in an artifact against
//!   the config's route table. Any disagreement is a named drift finding and a
//!   FAILURE: two artifacts that disagree cannot both describe this campaign.
//! - **The gate is mirrored, not re-invented.** Where this page reads the same
//!   input `limen verdict` reads, it takes the same position — including on
//!   what an absent metric family is allowed to mean (see [`FAMILIES`]). A page
//!   stricter than the gate it reports on renders FAILURE against runs the gate
//!   passed, and is worth no more than one that is laxer.
//! - **The page always exists.** Producing it is exit 0 even when it renders
//!   nothing but failures, because a CI artifact that vanishes on a bad run is
//!   a CI artifact nobody looks at. Only a page that could not be *produced*
//!   (an unwritable `--out`, an incoherent flag combination) is exit 1.
//!
//! The document is hand-built escaped HTML (the `render_human` precedent in
//! [`crate::verdict`]): one `<style>` block, no JavaScript, no external
//! references of any kind, and a text label on every state so it stays legible
//! without color. Every interpolated value goes through [`esc`] — route ids and
//! config strings arrive from documents this tool did not write.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::load::{load as load_config, ConfigOverrides};
use crate::config::model::{Config, FailSafeMode, RouteMode};
use crate::observability::prometheus::{
    BREAKER_TRANSITIONS_TOTAL, CIRCUIT_BREAKER_STATE, COMPARISONS_TOTAL, COMPARISON_SKIPPED_TOTAL,
    DIFF_SINK_DROPPED_TOTAL, DIFF_SINK_ENQUEUED_TOTAL, DIFF_SINK_WRITTEN_TOTAL,
    FLAG_CONSECUTIVE_FAILURES, FLAG_PROVIDER_STALE, FLAG_STALENESS_SECONDS, REQUESTS_TOTAL,
    ROLLOUT_RESOLVED_TARGET_PERCENTAGE, SHADOW_FAILED_TOTAL, SHADOW_IN_FLIGHT,
    SHADOW_SKIPPED_TOTAL, SHADOW_TOTAL,
};
use crate::observability::sink::{self, Report, ReportFilter, REPORT_EXAMPLES_PER_ROUTE};
use crate::resilience::BreakerState;
use crate::routing::Upstream;
use crate::verdict::{Sample, Scrape, CANARY_ROUTE_ID, RESERVED_ROUTE_ID_PREFIX};

/// The metric families the runtime-counters section renders, each tagged with
/// what an *absent* family is allowed to mean — mirroring
/// [`crate::verdict`]'s contract exactly rather than restating it:
///
/// - [`Absence::Required`] is [`crate::verdict::REQUIRED_SERIES`], the four
///   series `register_verdict_series` pre-touches at startup. Absent, a verdict
///   is exit 50; absent, this section is unavailable.
/// - [`Absence::ReadsAsZero`] is what `verdict::evaluate_floors` does with an
///   absent `limen_comparisons_total`: reads it as zero, which is fail-closed
///   only because a floored route needs at least one comparison to pass.
/// - [`Absence::Informational`] is what `verdict::collect_informational` does:
///   iterate whatever is there and gate on none of it.
///
/// Every family but the required four is registered *lazily*, on the first
/// event of its kind, so a proxy that never skipped a comparison exports no
/// `limen_comparison_skipped_total` at all. Requiring those families made this
/// page stricter than the gate it claims to mirror — every quiet, healthy
/// service rendered FAILURE.
const FAMILIES: [(&str, Absence); 9] = [
    (COMPARISONS_TOTAL, Absence::ReadsAsZero),
    (COMPARISON_SKIPPED_TOTAL, Absence::Informational),
    (SHADOW_TOTAL, Absence::Informational),
    (SHADOW_SKIPPED_TOTAL, Absence::Informational),
    (SHADOW_FAILED_TOTAL, Absence::Informational),
    (SHADOW_IN_FLIGHT, Absence::Required),
    (DIFF_SINK_ENQUEUED_TOTAL, Absence::Required),
    (DIFF_SINK_WRITTEN_TOTAL, Absence::Required),
    (DIFF_SINK_DROPPED_TOTAL, Absence::Required),
];

/// How many sink examples the page shows per route.
const EXAMPLES_SHOWN: usize = 3;

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// The artifacts one page is rendered from. Only the sink directory is
/// positional-required by the CLI; the rest are paths the operator captured.
#[derive(Debug, Clone)]
pub struct Inputs {
    /// Sink directory holding `mismatches-<date>.jsonl` files.
    pub sink_dir: PathBuf,
    /// The limen configuration the campaign ran under.
    pub config: Option<PathBuf>,
    /// A file captured from `limen verdict --format json`.
    pub verdict: Option<PathBuf>,
    /// A saved `GET /observe/profile` body.
    pub profile: Option<PathBuf>,
    /// A saved `/metrics` text scrape.
    pub metrics: Option<PathBuf>,
}

/// One input's standing on the page. `Unavailable` carries the reason so the
/// manifest can say *why* rather than just that something went wrong.
#[derive(Debug, Clone, PartialEq)]
pub enum Section<T> {
    /// The flag was not passed at all.
    NotProvided,
    /// Read and parsed.
    Ok(T),
    /// Provided but unreadable or unparsable.
    Unavailable(String),
}

impl<T> Section<T> {
    /// The parsed value, if there is one.
    pub fn get(&self) -> Option<&T> {
        match self {
            Section::Ok(v) => Some(v),
            _ => None,
        }
    }

    /// Whether this input was provided but could not be used.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Section::Unavailable(_))
    }

    /// The status word shown in the inputs manifest.
    fn word(&self) -> &'static str {
        match self {
            Section::NotProvided => "NOT PROVIDED",
            Section::Ok(_) => "PARSED",
            Section::Unavailable(_) => "UNAVAILABLE",
        }
    }

    /// The CSS state class matching [`Section::word`].
    fn class(&self) -> &'static str {
        match self {
            Section::NotProvided => "neutral",
            Section::Ok(_) => "good",
            Section::Unavailable(_) => "bad",
        }
    }
}

// ---------------------------------------------------------------------------
// Read-side DTOs
// ---------------------------------------------------------------------------

/// A verdict artifact, in whichever of its two shapes it arrived.
#[derive(Debug, Clone, PartialEq)]
pub enum VerdictArtifact {
    /// The ad-hoc exit-50 document `limen verdict` prints when a required
    /// input was unavailable. Distinguished by `mode: "unavailable"` before
    /// anything else is attempted: it shares no structure with a real verdict,
    /// and reading it through the generic bucket would render a tooling failure
    /// as an empty-but-valid report.
    InputUnavailable(UnavailableDto),
    /// The full `VerdictReport` document. Boxed: it dwarfs the other variant,
    /// and every input here is held by value on the page model.
    Full(Box<VerdictDto>),
}

impl VerdictArtifact {
    /// The document, when it arrived as a full verdict rather than the exit-50
    /// shape. Everything downstream of a verdict asks this question.
    fn full(&self) -> Option<&VerdictDto> {
        match self {
            VerdictArtifact::Full(v) => Some(v),
            VerdictArtifact::InputUnavailable(_) => None,
        }
    }
}

/// The exit-50 shape (`cli::render_unavailable`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct UnavailableDto {
    /// Always `"unavailable"` — the discriminator.
    pub mode: String,
    /// Always `"input-unavailable"`.
    #[serde(default)]
    pub verdict: String,
    /// Always 50.
    #[serde(default)]
    pub exit_code: u8,
    /// What was unavailable.
    #[serde(default)]
    pub error: String,
}

/// The read side of [`crate::verdict::VerdictReport`].
///
/// Deliberately looser than the writer (the [`sink::ReportRecord`] precedent):
/// unknown fields are ignored so a verdict written by a newer limen still
/// renders. The six fields the banner's semantics rest on are required, though
/// — defaulting them would let any JSON object at all parse as a verdict, and
/// an empty object would then read as "clean, nothing found".
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct VerdictDto {
    /// `"online"` or `"offline"`.
    pub mode: String,
    /// The typed verdict name (`clean`, `mismatches-found`, …).
    pub verdict: String,
    /// The documented process exit code.
    pub exit_code: u8,
    /// The five named checks.
    pub checks: ChecksDto,
    /// Non-canary mismatch records the verdict counted in the sink.
    pub mismatches_total: u64,
    /// Per-floored-route standing.
    pub floors: Vec<FloorDto>,
    /// Canary records (excluded from `mismatches_total`).
    #[serde(default)]
    pub canary_records: u64,
    /// Per-route sink mismatch counts, reconciled against the sink here.
    #[serde(default)]
    pub sink_mismatches_by_route: BTreeMap<String, u64>,
    /// Non-gating skip/failure counters.
    #[serde(default)]
    pub informational: Vec<InfoDto>,
}

/// The five named checks, in report order.
///
/// All five are **required**, and so are both fields of each. Defaulting them
/// would let `{"checks": {"sink_integrity": {…}}}` parse into four
/// silently-empty checks — statuses that are not `"fail"`, and so pass every
/// contradiction test below while standing for nothing. A verdict that does not
/// carry all five checks is not a verdict this page can read.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ChecksDto {
    pub drain: CheckDto,
    pub floors: CheckDto,
    pub sink_integrity: CheckDto,
    pub canary: CheckDto,
    pub mismatches: CheckDto,
}

impl ChecksDto {
    /// The checks with their names, in report order.
    fn named(&self) -> [(&'static str, &CheckDto); 5] {
        [
            ("drain", &self.drain),
            ("floors", &self.floors),
            ("sink integrity", &self.sink_integrity),
            ("canary", &self.canary),
            ("mismatches", &self.mismatches),
        ]
    }
}

/// One check's outcome. Both fields required — see [`ChecksDto`].
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct CheckDto {
    /// `pass` / `fail` / `skipped`; anything else is rendered verbatim and
    /// never treated as a pass.
    pub status: String,
    pub detail: String,
}

impl CheckDto {
    fn is_fail(&self) -> bool {
        self.status == "fail"
    }
    fn is_pass(&self) -> bool {
        self.status == "pass"
    }
    fn is_skipped(&self) -> bool {
        self.status == "skipped"
    }

    /// The check's own state pill, decided by the check's status and nothing
    /// else — a failed check is red whatever exit code the document claims
    /// beside it. A status this page does not recognize is rendered as it
    /// arrived and colored as a failure: an unknown word is not a pass.
    fn status_pill(&self) -> String {
        match self.status.as_str() {
            "pass" => pill("good", "PASS"),
            "fail" => pill("bad", "FAIL"),
            "skipped" => pill("warn", "SKIPPED"),
            "" => pill("bad", "NO STATUS"),
            other => pill("bad", other),
        }
    }
}

/// One floored route's standing.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct FloorDto {
    pub route_id: String,
    pub comparisons: u64,
    pub floor: u64,
    pub met: bool,
}

/// One informational counter row.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct InfoDto {
    pub metric: String,
    pub route: String,
    pub reason: String,
    pub value: u64,
}

/// The read side of [`crate::observability::observe::ObserveProfile`]. The two
/// container fields are required (a document without them is not a profile);
/// every per-route field defaults, so a profile from a newer limen renders with
/// the counters this binary knows about.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProfileDto {
    pub sample_rate: f64,
    pub routes: BTreeMap<String, RouteProfileDto>,
}

/// The counters this page renders out of
/// [`crate::observability::observe::RouteProfile`]. Only those: a field nobody
/// renders is a field nobody checked, and carrying it here would suggest the
/// page had looked at it. The overflow flags are kept without their collections
/// because the flag is the caveat — it says the profile itself is incomplete,
/// which is the one thing a fail-closed page must not lose.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct RouteProfileDto {
    pub observations: u64,
    pub reads: u64,
    pub writes: u64,
    pub transport_errors: u64,
    pub methods: BTreeMap<String, u64>,
    pub query_names_overflow: bool,
    pub distinct_read_paths_overflow: bool,
    pub status_classes: BTreeMap<String, u64>,
    pub content_types_overflow: bool,
    pub set_cookie_reads: u64,
    pub redirect_reads: u64,
    pub length_missing: u64,
    pub fingerprint_overflow: bool,
}

// ---------------------------------------------------------------------------
// Derived views
// ---------------------------------------------------------------------------

/// What the page needs from a config: the route table and, per route, whether
/// the verdict is expected to carry a floors row for it.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigView {
    pub routes: Vec<ConfigRoute>,
    /// `flags.fail_safe_mode` — what a stale flag provider displaces every
    /// rollout with. Carried because the rollout section states the *joined*
    /// truth ("0% because the flags are stale"), and that sentence is only
    /// honest if the page read which fail-safe the config actually declares.
    pub fail_safe_mode: FailSafeMode,
    /// `flags.stale_ttl_ms` — the threshold the stale gauge is set against, so
    /// the page can tell one coherent provider snapshot from three gauges that
    /// cannot have come from one (see [`flag_tuple_fault`]).
    pub stale_ttl_ms: u64,
}

/// One configured route, as the page renders it.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigRoute {
    pub id: String,
    pub mode: RouteMode,
    pub comparison_enabled: bool,
    pub sample_rate: f64,
    /// `comparison.effective_min_comparisons()`.
    pub floor: u64,
    /// Whether `evaluate_floors` would include this route — comparison enabled
    /// *and* a non-zero effective floor. Mirrors that filter so the page can
    /// tell a route the verdict legitimately omits from one it lost.
    pub expects_floor_row: bool,
    /// The `rollout:` block, for the modes that carry one.
    pub rollout: Option<RolloutSettings>,
    /// The `circuit_breaker:` block, tuning included: the numbers that decide
    /// what an open breaker on this route even means.
    pub breaker: BreakerSettings,
    /// Whether a failed in-flight request may be replayed against legacy
    /// (safety invariant 4).
    pub failover_safe: bool,
}

/// A route's `rollout:` block.
#[derive(Debug, Clone, PartialEq)]
pub struct RolloutSettings {
    pub percentage_flag: String,
    pub default_percentage: f64,
    /// `assignment_key.header`, when the key comes from one.
    pub assignment_header: Option<String>,
    /// What happens when that header is absent.
    pub assignment_fallback: String,
}

/// A route's `circuit_breaker:` block.
#[derive(Debug, Clone, PartialEq)]
pub struct BreakerSettings {
    pub enabled: bool,
    pub failure_rate_threshold: f64,
    pub min_requests: u32,
    pub open_duration_ms: u64,
    pub half_open_max_requests: u32,
}

impl ConfigRoute {
    /// Whether this route runs one of the two modes the rollout section reports
    /// on — the modes whose requests reach the new-upstream gate, which is
    /// where a rollout target and a breaker are decided at all.
    fn is_rollout_route(&self) -> bool {
        self.mode.gates_new()
    }

    /// Whether a target-percentage series must exist for this route — the
    /// config-side mirror of [`crate::routing::CompiledRoute::rollout_target`],
    /// which is what decides the registered set. Mirrored rather than shared:
    /// this side reads a raw [`Config`], and a `CompiledRoute` exists only
    /// after the routing table compiles.
    fn expects_target_series(&self) -> bool {
        self.mode == RouteMode::PercentageSplit && self.rollout.is_some()
    }

    /// Whether the four transition counters must exist — the config-side mirror
    /// of [`crate::routing::CompiledRoute::breaker_consulted`], down to the
    /// shared [`RouteMode::gates_new`] the two both ask. Having a breaker is not
    /// enough: a `legacy_only` route may configure one no request will ever ask.
    fn breaker_consulted(&self) -> bool {
        self.breaker.enabled && self.is_rollout_route()
    }
}

impl ConfigView {
    fn from_config(config: &Config) -> ConfigView {
        ConfigView {
            fail_safe_mode: config.flags.fail_safe_mode,
            stale_ttl_ms: config.flags.stale_ttl_ms,
            routes: config
                .routes
                .iter()
                .map(|r| ConfigRoute {
                    id: r.id.clone(),
                    mode: r.mode,
                    comparison_enabled: r.comparison.enabled,
                    sample_rate: r.comparison.sample_rate,
                    floor: r.comparison.effective_min_comparisons(),
                    expects_floor_row: r.comparison.enabled
                        && r.comparison.effective_min_comparisons() > 0,
                    rollout: r.rollout.as_ref().map(|rollout| RolloutSettings {
                        percentage_flag: rollout.percentage_flag.clone(),
                        default_percentage: rollout.default_percentage,
                        assignment_header: rollout.assignment_key.header.clone(),
                        assignment_fallback: match rollout.assignment_key.fallback {
                            crate::config::model::AssignmentFallback::RequestRandom => {
                                "request_random".to_string()
                            }
                        },
                    }),
                    breaker: BreakerSettings {
                        enabled: r.circuit_breaker.enabled,
                        failure_rate_threshold: r.circuit_breaker.failure_rate_threshold,
                        min_requests: r.circuit_breaker.min_requests,
                        open_duration_ms: r.circuit_breaker.open_duration_ms,
                        half_open_max_requests: r.circuit_breaker.half_open_max_requests,
                    },
                    failover_safe: r.failover_safe,
                })
                .collect(),
        }
    }

    fn route(&self, id: &str) -> Option<&ConfigRoute> {
        self.routes.iter().find(|r| r.id == id)
    }

    /// The routes the rollout section reports on, in config order.
    fn rollout_routes(&self) -> impl Iterator<Item = &ConfigRoute> {
        self.routes.iter().filter(|r| r.is_rollout_route())
    }
}

/// What `limen verdict` does with a family the scrape does not carry. See
/// [`FAMILIES`] for the mapping and the code each arm mirrors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Absence {
    /// Fails the whole read closed ([`crate::verdict::REQUIRED_SERIES`]).
    Required,
    /// Read as zero, which is only safe because a floor of at least one turns
    /// that zero into a failure (`verdict::evaluate_floors`).
    ReadsAsZero,
    /// Never gated on (`verdict::collect_informational`).
    Informational,
}

impl Absence {
    /// What the page says beside an absent family, in the page's own voice.
    /// Never silence: an absent counter is a fact about the scrape, and a fact
    /// a fail-closed page states rather than omits.
    fn note(self) -> &'static str {
        match self {
            // Unreachable in a rendered section — a required family absent
            // makes the whole section unavailable — but stated for symmetry.
            Absence::Required => {
                "absent — this series is registered at startup, so its absence means the \
                 scrape did not come from a limen control plane"
            }
            Absence::ReadsAsZero => {
                "absent from the scrape. `limen verdict` reads this as zero comparisons, which \
                 is fail-closed only because a floored route needs at least one — the coverage \
                 table above is where that bites, not here"
            }
            Absence::Informational => {
                "absent from the scrape. This counter is registered on the first event of its \
                 kind, so no such event was recorded by the scraped process. `limen verdict` \
                 gates on none of these"
            }
        }
    }
}

/// The runtime counters, flattened out of a scrape into renderable rows.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricsView {
    pub families: Vec<MetricFamily>,
}

/// One metric family's standing in the scrape.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricFamily {
    pub name: String,
    /// What this family's absence would mean.
    pub absence: Absence,
    /// Whether the scrape carried the family at all. `false` here is only ever
    /// a tolerated absence: a required family absent is a section-level
    /// unavailable, never a row.
    pub present: bool,
    pub rows: Vec<MetricRow>,
}

/// One sample, split into the route label and everything else.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricRow {
    /// The `route` label, when the series carries one — the pipeline counters
    /// are process-wide and do not.
    pub route: Option<String>,
    /// The remaining labels, `k=v` joined — `reason=event_stream`, and so on.
    pub labels: String,
    pub value: u64,
}

impl MetricsView {
    /// Build the view, or say why the scrape cannot be rendered.
    ///
    /// Absence is tolerated exactly where `limen verdict` tolerates it (see
    /// [`FAMILIES`]) and rendered as an explicit note rather than passed over
    /// in silence. Values are *not* tolerated the same way: a count that is not
    /// an exact non-negative integer is never a normal state of a limen
    /// exporter, and rounding one would be fabricating a number.
    fn from_scrape(scrape: &Scrape) -> Result<MetricsView, String> {
        let mut families = Vec::with_capacity(FAMILIES.len());
        for (name, absence) in FAMILIES {
            let present = scrape.has_family(name);
            if !present && absence == Absence::Required {
                return Err(format!(
                    "required metric family {name} is absent from the scrape — limen registers \
                     it at startup, so its absence is a scrape of something else, never a zero \
                     count"
                ));
            }
            let mut rows = Vec::new();
            for sample in scrape.family(name) {
                // The raw token, never the parsed `f64`: a counter is an exact
                // integer, and `2^64` reads back off an `f64` as finite,
                // integral and non-negative before saturating to `u64::MAX` on
                // cast — a fabricated count on an otherwise green page. A value
                // this page cannot represent exactly is a refusal.
                let Ok(value) = sample.raw_value.parse::<u64>() else {
                    return Err(format!(
                        "{name} carries {:?}, which is not an exact non-negative integer count \
                         that fits a 64-bit counter — refusing to round or saturate it",
                        sample.raw_value
                    ));
                };
                let labels = sample
                    .labels
                    .iter()
                    .filter(|(k, _)| k.as_str() != "route")
                    .map(|(k, val)| format!("{k}={val}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                rows.push(MetricRow {
                    route: sample.labels.get("route").cloned(),
                    labels,
                    value,
                });
            }
            rows.sort_by(|a, b| a.route.cmp(&b.route).then_with(|| a.labels.cmp(&b.labels)));
            families.push(MetricFamily {
                name: name.to_string(),
                absence,
                present,
                rows,
            });
        }
        Ok(MetricsView { families })
    }

    /// Every route id the counters mention.
    fn route_ids(&self) -> BTreeSet<String> {
        self.families
            .iter()
            .flat_map(|f| f.rows.iter())
            .filter_map(|r| r.route.clone())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Rollout and resilience
//
// A second, typed reading of the *same* parsed scrape the counters section
// renders — never a second scrape, and deliberately not routed through
// [`MetricsView`]: that view's contract is an exact-`u64` count per sample, and
// three of the families below are floats (a percentage, a staleness in
// seconds). Widening it to hold them would loosen the one property it exists
// for.
//
// The discipline here is per-*route*, not per-family: one route's torn series
// makes that route's row unavailable and leaves every other row standing,
// because "we cannot say what route X was doing" is a different claim from "we
// cannot say what any of this was doing", and collapsing them costs the page
// the routes it could still have reported honestly.
// ---------------------------------------------------------------------------

/// Every family this section reads. Their *absence as a set* is what tells a
/// scrape from a limen that predates rollout truth from one that carries it.
const ROLLOUT_FAMILIES: [&str; 6] = [
    ROLLOUT_RESOLVED_TARGET_PERCENTAGE,
    CIRCUIT_BREAKER_STATE,
    BREAKER_TRANSITIONS_TOTAL,
    FLAG_PROVIDER_STALE,
    FLAG_STALENESS_SECONDS,
    FLAG_CONSECUTIVE_FAILURES,
];

/// One cell of the rollout table. The third variant is the point of the type:
/// a value the scrape could not settle is rendered as unsettled, never as the
/// zero it would default to.
#[derive(Debug, Clone, PartialEq)]
pub enum Reading<T> {
    /// Read from the scrape and validated.
    Known(T),
    /// The config says this cell cannot exist — a failover route has no
    /// rollout target, a breakerless route has no breaker state.
    NotApplicable(&'static str),
    /// The scrape does not settle it, and why.
    Unknown(String),
}

impl<T> Reading<T> {
    fn known(&self) -> Option<&T> {
        match self {
            Reading::Known(value) => Some(value),
            _ => None,
        }
    }
}

/// What the flag-provider gauges said at scrape time. Process-wide: one
/// provider serves every route.
#[derive(Debug, Clone, PartialEq)]
pub struct FlagProviderTruth {
    /// `limen_flag_provider_stale` — 1 means every rollout is displaced by the
    /// configured fail-safe, whatever the flags say.
    pub stale: bool,
    /// Age of the last successful refresh. `None` is the exporter's `-1`
    /// sentinel: there has never been one.
    pub staleness_seconds: Option<f64>,
    pub consecutive_failures: u64,
}

/// The share of a route's traffic each upstream actually served, as counted by
/// `limen_requests_total`. The rollout's *target* is what it asked for; this is
/// what happened.
///
/// Each side is `None` when the scrape carries no series for that upstream at
/// all. That is **not** a zero and is not rendered as one: this counter is
/// registered on the first request of its kind, so an absent side is
/// "zero recorded, and nothing registered the series to say so" — a distinction
/// a rollout at 0% (which legitimately has no `new` side) turns on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedShare {
    pub new: Option<u64>,
    pub legacy: Option<u64>,
}

impl ObservedShare {
    /// New's share, treating an absent side as the zero it stands for — but
    /// only ever rendered beside the annotation that says the side was absent
    /// (see [`observed_cell`]). `None` when neither side counted anything: a
    /// share of no traffic is not 0%.
    pub fn percentage(self) -> Option<f64> {
        // `u128`: two `u64` counts cannot overflow it, so the denominator has
        // no panic path and no saturation whatever a scrape carries.
        let new = self.new.unwrap_or(0) as u128;
        let total = new + self.legacy.unwrap_or(0) as u128;
        (total > 0).then(|| new as f64 * 100.0 / total as f64)
    }

    /// The sides the scrape carried no series for, in render order.
    pub fn missing_sides(self) -> Vec<&'static str> {
        [(self.new, "new"), (self.legacy, "legacy")]
            .iter()
            .filter(|(count, _)| count.is_none())
            .map(|(_, name)| *name)
            .collect()
    }
}

/// One route's rollout standing: what it targeted, what it served, and what its
/// breaker was doing.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteTruth {
    /// The resolved target percentage from the gauge — never the config's
    /// `default_percentage`, which is what the rollout would fall back to
    /// rather than what it resolved to.
    pub target: Reading<f64>,
    pub observed: Reading<ObservedShare>,
    pub breaker_state: Reading<BreakerState>,
    /// The four legal transitions, in [`BreakerState::TRANSITIONS`] order.
    pub transitions: Reading<[u64; 4]>,
    /// Set when the gauge and the transition history disagree by exactly one
    /// legal step — the benign race between the scrape handler's gauge refresh
    /// and the exposition render. Carries the state the *counters* imply, and
    /// makes the cell say so rather than presenting either reading as settled.
    pub state_skew: Option<BreakerState>,
}

/// One row of the rollout table: the route as configured, plus what the scrape
/// says about it.
#[derive(Debug, Clone, PartialEq)]
pub struct RolloutRow {
    pub id: String,
    pub mode: RouteMode,
    pub failover_safe: bool,
    pub rollout: Option<RolloutSettings>,
    pub breaker: BreakerSettings,
    pub breaker_consulted: bool,
    /// Why this row cannot be believed. Non-empty means the row renders
    /// unavailable — its cells are not rendered at all — and every entry
    /// becomes a banner failure.
    pub rejected: Vec<String>,
    pub truth: RouteTruth,
}

/// What the section was able to check against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutScope {
    /// The config declares rollout routes, and the rows were checked against
    /// exactly that set.
    Configured,
    /// The config declares neither rollout mode: an honest empty, not a
    /// finding.
    NoRolloutRoutes,
    /// No config parsed, so the route set the scrape *should* carry is unknown
    /// and no per-route presence could be required. The process-wide flag
    /// gauges are still readable.
    Unchecked,
}

/// The flag-provider block's standing. Split out of [`Reading`] because its
/// two failure modes carry different weight: a gauge the scrape does not have
/// is only owed where a config proves there was a rollout, while three gauges
/// that contradict each other are corruption wherever they turn up — limen
/// derives all three from **one** `health()` snapshot, so no live provider can
/// produce a tuple that disagrees with itself.
#[derive(Debug, Clone, PartialEq)]
pub enum FlagReading {
    /// No route runs a rollout, so no provider health is owed.
    NotApplicable,
    Known(FlagProviderTruth),
    /// A gauge is absent, duplicated, or not a number.
    Absent(String),
    /// The three gauges cannot have come from one snapshot.
    Contradiction(String),
}

impl FlagReading {
    fn known(&self) -> Option<&FlagProviderTruth> {
        match self {
            FlagReading::Known(truth) => Some(truth),
            _ => None,
        }
    }
}

/// The rollout section's model.
#[derive(Debug, Clone, PartialEq)]
pub struct RolloutResilienceView {
    pub scope: RolloutScope,
    pub flags: FlagReading,
    pub rows: Vec<RolloutRow>,
    /// `flags.fail_safe_mode`, for the sentence a stale provider earns.
    pub fail_safe_mode: Option<FailSafeMode>,
    /// Rollout series the config cannot account for: an unknown route label, or
    /// a series on a route that cannot own one. Either way the scrape and the
    /// config describe different deployments.
    pub stray: Vec<String>,
}

impl RolloutResilienceView {
    /// The honest empty.
    fn none_configured() -> RolloutResilienceView {
        RolloutResilienceView {
            scope: RolloutScope::NoRolloutRoutes,
            flags: FlagReading::NotApplicable,
            rows: Vec::new(),
            fail_safe_mode: None,
            stray: Vec::new(),
        }
    }

    /// What the banner must treat as a failure.
    ///
    /// Three kinds, and none of them may ride under a green banner:
    /// - A **stale provider**: every `percentage_split` route was displaced by
    ///   the fail-safe, so the rollout in the config is not the rollout that
    ///   ran.
    /// - A **diverting breaker**: an open breaker means the new upstream is not
    ///   being exercised at all and a half-open one means it is on probation.
    ///   Either is a campaign whose comparison coverage is not what the config
    ///   asked for, however clean the diffs look.
    /// - A **row the page could not read**, or a series the config cannot
    ///   account for.
    pub fn failures(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.scope == RolloutScope::NoRolloutRoutes {
            return out;
        }
        match &self.flags {
            // Absence is only demanded where the config proves there was a
            // rollout to report on; a contradiction needs no such licence.
            FlagReading::Absent(why) if self.scope == RolloutScope::Configured => {
                out.push(format!("rollout: {why}"));
            }
            FlagReading::Contradiction(why) => out.push(format!("rollout: {why}")),
            FlagReading::Known(flags) if flags.stale => out.push(format!(
                "rollout: the flag provider is stale — every percentage_split route is displaced \
                 by fail-safe {}, so the rollout that ran is not the one this config describes",
                self.fail_safe_mode.map_or("mode", fail_safe_name)
            )),
            _ => {}
        }
        out.extend(self.stray.iter().map(|why| format!("rollout: {why}")));
        for row in &self.rows {
            for why in &row.rejected {
                out.push(format!("rollout: route {}: {why}", row.id));
            }
            // The reported state, which under a one-step skew is the more
            // diverting of the two readings — a breaker caught mid-transition
            // is not an excuse to report the calmer end of it.
            if let Reading::Known(state) = &row.truth.breaker_state {
                match state {
                    BreakerState::Open => out.push(format!(
                        "rollout: breaker open on route {} — the new upstream is blocked and \
                         every request on this route is being served by legacy",
                        row.id
                    )),
                    BreakerState::HalfOpen => out.push(format!(
                        "rollout: breaker half-open on route {} — the new upstream is on \
                         probation and only trial requests reach it",
                        row.id
                    )),
                    BreakerState::Closed => {}
                }
            }
        }
        out
    }
}

/// The configured fail-safe, in the exposition's own vocabulary.
fn fail_safe_name(mode: FailSafeMode) -> &'static str {
    match mode {
        FailSafeMode::LegacyOnly => "legacy_only",
    }
}

/// The samples of `family` carrying this route id.
fn route_samples<'a>(scrape: &'a Scrape, family: &'a str, route: &str) -> Vec<&'a Sample> {
    scrape
        .family(family)
        .filter(|s| s.labels.get("route").map(String::as_str) == Some(route))
        .collect()
}

/// A gauge's raw token as a finite float. The raw token, never `Sample::value`:
/// the same reasoning as [`MetricsView::from_scrape`] — a value this page
/// cannot represent is a refusal, not a rounding.
fn finite(family: &str, raw: &str) -> Result<f64, String> {
    match raw.parse::<f64>() {
        Ok(value) if value.is_finite() => Ok(value),
        _ => Err(format!(
            "{family} carries {raw:?}, which is not a finite number"
        )),
    }
}

/// An exact non-negative integer count, on the [`MetricsView`] contract.
fn exact_u64(family: &str, raw: &str) -> Result<u64, String> {
    raw.parse::<u64>().map_err(|_| {
        format!(
            "{family} carries {raw:?}, which is not an exact non-negative integer count that fits \
             a 64-bit counter — refusing to round or saturate it"
        )
    })
}

/// The one series of `family` for this route, or why there is not exactly one.
fn sole_series<'a>(scrape: &'a Scrape, family: &'a str, route: &str) -> Result<&'a Sample, String> {
    match route_samples(scrape, family, route).as_slice() {
        [one] => Ok(one),
        [] => Err(format!(
            "the scrape carries no {family} series for this route — limen registers it at \
             startup, so its absence is a scrape that cannot answer for the route, never a zero"
        )),
        many => Err(format!(
            "the scrape carries {} {family} series for this route — more than one cannot be \
             reconciled into a single reading, and picking or summing them would be inventing the \
             answer",
            many.len()
        )),
    }
}

/// The resolved rollout target: exactly one series, a finite percentage in
/// `0..=100`.
fn read_target(scrape: &Scrape, route: &str) -> Result<f64, String> {
    let sample = sole_series(scrape, ROLLOUT_RESOLVED_TARGET_PERCENTAGE, route)?;
    let value = finite(ROLLOUT_RESOLVED_TARGET_PERCENTAGE, &sample.raw_value)?;
    if !(0.0..=100.0).contains(&value) {
        return Err(format!(
            "{ROLLOUT_RESOLVED_TARGET_PERCENTAGE} reads {:?}, which is not a percentage in \
             0..=100 — the resolver clamps into that range, so a value outside it did not come \
             from one",
            sample.raw_value
        ));
    }
    Ok(value)
}

/// The breaker state gauge for a route's **new** upstream.
///
/// Required, not optional: limen's `/metrics` handler writes this gauge for
/// every route that has a breaker, on every scrape. Its absence beside a
/// breaker-consulted route is therefore a torn or foreign scrape — and a
/// breaker whose state cannot be read is exactly the one that must not render
/// as a quiet "closed".
///
/// The `upstream` label must be `new`: the breaker guards that upstream and
/// nothing else, so a sole series carrying another label — or none — is a
/// series limen did not write, and reading it as the breaker's state would be
/// answering with somebody else's number.
fn read_breaker_state(scrape: &Scrape, route: &str) -> Result<BreakerState, String> {
    let samples = route_samples(scrape, CIRCUIT_BREAKER_STATE, route);
    if let Some(odd) = samples
        .iter()
        .find(|s| s.labels.get("upstream").map(String::as_str) != Some(Upstream::New.as_str()))
    {
        return Err(format!(
            "the scrape carries a {CIRCUIT_BREAKER_STATE} series for this route with upstream={:?} \
             — the breaker guards the new upstream, so a state under any other label is not this \
             breaker's",
            odd.labels.get("upstream").cloned().unwrap_or_default()
        ));
    }
    let sample = match samples.as_slice() {
        [] => {
            return Err(format!(
                "the scrape carries no {CIRCUIT_BREAKER_STATE} series for this route — limen \
                 writes this gauge on every scrape for every route that has a breaker, so its \
                 absence is a scrape that cannot say whether the breaker was diverting, never a \
                 closed one"
            ))
        }
        [one] => one,
        many => {
            return Err(format!(
                "the scrape carries {} {CIRCUIT_BREAKER_STATE} series for this route — a breaker \
                 is in one state, and reconciling two readings into one would be inventing it",
                many.len()
            ))
        }
    };
    // The gauge is an enum written as a float, so the three legal readings are
    // compared exactly: 1.5 is not "roughly half-open", it is a value limen's
    // exporter cannot have written.
    let value = finite(CIRCUIT_BREAKER_STATE, &sample.raw_value)?;
    for (encoded, state) in [
        (0.0, BreakerState::Closed),
        (1.0, BreakerState::HalfOpen),
        (2.0, BreakerState::Open),
    ] {
        if value == encoded {
            return Ok(state);
        }
    }
    Err(format!(
        "{CIRCUIT_BREAKER_STATE} reads {:?}, which is not one of 0 (closed), 1 (half-open) or 2 \
         (open)",
        sample.raw_value
    ))
}

/// The `from`→`to` pair a transition series carries. Absent labels read as
/// empty rather than being skipped: a series without them is still a series
/// [`read_transitions`]'s legality check has to refuse.
fn transition_pair(sample: &Sample) -> (&str, &str) {
    (
        sample.labels.get("from").map(String::as_str).unwrap_or(""),
        sample.labels.get("to").map(String::as_str).unwrap_or(""),
    )
}

/// The four transition counters, in [`BreakerState::TRANSITIONS`] order. All
/// four are registered at startup for a breaker-consulted route, so a missing
/// one is a scrape that cannot answer rather than a breaker that never moved.
fn read_transitions(scrape: &Scrape, route: &str) -> Result<[u64; 4], String> {
    let samples = route_samples(scrape, BREAKER_TRANSITIONS_TOTAL, route);
    let mut counts = [0u64; 4];
    for (index, (from, to)) in BreakerState::TRANSITIONS.iter().enumerate() {
        let matching: Vec<&Sample> = samples
            .iter()
            .copied()
            .filter(|s| transition_pair(s) == (from.as_str(), to.as_str()))
            .collect();
        counts[index] = match matching.as_slice() {
            [one] => exact_u64(BREAKER_TRANSITIONS_TOTAL, &one.raw_value)?,
            [] => {
                return Err(format!(
                    "the breaker is consulted here but the scrape carries no \
                     {BREAKER_TRANSITIONS_TOTAL} series for {}→{} — all four are registered at \
                     startup, so a missing one is not a transition that never happened",
                    from.as_str(),
                    to.as_str()
                ))
            }
            many => {
                return Err(format!(
                    "the scrape carries {} {BREAKER_TRANSITIONS_TOTAL} series for {}→{} — summing \
                     them would be inventing a transition count",
                    many.len(),
                    from.as_str(),
                    to.as_str()
                ))
            }
        };
    }
    // A pair the state machine cannot make means this is not limen's breaker
    // being described, and the four counts above cannot be trusted to be its
    // whole story.
    for sample in &samples {
        let pair = transition_pair(sample);
        let legal = BreakerState::TRANSITIONS
            .iter()
            .any(|(from, to)| (from.as_str(), to.as_str()) == pair);
        if !legal {
            return Err(format!(
                "the scrape carries a {BREAKER_TRANSITIONS_TOTAL} series for {}→{}, which is not \
                 a transition limen's breaker can make",
                pair.0, pair.1
            ));
        }
    }
    Ok(counts)
}

/// The state a transition history *implies*, or why no history could have
/// produced these four counts.
///
/// A breaker starts closed, so each state's occupancy is its entries minus its
/// exits (plus one for closed, which is where the machine begins):
///
/// ```text
/// open      = (closed→open) + (half_open→open) − (open→half_open)
/// half_open = (open→half_open) − (half_open→closed) − (half_open→open)
/// closed    = 1 + (half_open→closed) − (closed→open)
/// ```
///
/// The three sum to 1 algebraically, so the whole consistency question is
/// whether each one lands in `{0, 1}`. A negative occupancy means a state was
/// exited more often than it was entered; a 2 means it was entered twice
/// without leaving. Either is a tuple no run of limen's state machine can
/// produce — it is a hand-edited, merged, or foreign scrape, and the counts
/// beside it cannot be believed either. Signed arithmetic throughout: these are
/// differences of counters, and `u64` subtraction would wrap a contradiction
/// into an enormous plausible-looking number.
fn state_from_transitions(counts: [u64; 4]) -> Result<BreakerState, String> {
    let [closed_open, open_half, half_closed, half_open] = counts.map(i128::from);
    let occupancy = [
        (
            BreakerState::Closed,
            1 + half_closed - closed_open,
            "closed",
        ),
        (
            BreakerState::HalfOpen,
            open_half - half_closed - half_open,
            "half-open",
        ),
        (
            BreakerState::Open,
            closed_open + half_open - open_half,
            "open",
        ),
    ];
    for (_, count, name) in &occupancy {
        if !(0..=1).contains(count) {
            return Err(format!(
                "the {BREAKER_TRANSITIONS_TOTAL} counts {counts:?} (in {}) describe no history a \
                 breaker can have: they leave it {count} times in the {name} state, and a breaker \
                 is in exactly one state at a time",
                transition_order()
            ));
        }
    }
    occupancy
        .iter()
        .find(|(_, count, _)| *count == 1)
        .map(|(state, _, _)| *state)
        // Unreachable: the three occupancies sum to 1 by construction, so one
        // of them is 1 once all three are known to be in {0, 1}. Stated as a
        // refusal rather than an unwrap — this page never guesses a state.
        .ok_or_else(|| {
            format!("the {BREAKER_TRANSITIONS_TOTAL} counts {counts:?} imply no state at all")
        })
}

/// The transition order the count tuples are printed in, named so an error
/// message about `[2, 1, 1, 0]` says which number is which.
fn transition_order() -> String {
    BreakerState::TRANSITIONS
        .iter()
        .map(|(from, to)| format!("{}→{}", from.as_str(), to.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Reconcile the state gauge against the state the transition counts imply.
///
/// `Ok(None)` is agreement. `Ok(Some(implied))` is a **one-step skew**, which
/// is benign and expected: the `/metrics` handler refreshes the state gauge for
/// every route and *then* renders the exposition, so a transition landing
/// inside that window is already counted but not yet in the gauge.
///
/// The tolerance is **directional** — the counters may be one transition *ahead*
/// of the gauge, never behind it. `CircuitBreaker::publish` increments the
/// counter with the state mutex still held, in the same critical section that
/// stores the new phase, so no reader can see a new state beside an
/// un-incremented counter. A symmetric tolerance would also be vacuous: the
/// four legal transitions connect all three states pairwise, so accepting
/// either direction would accept every mismatch there is.
///
/// Two steps apart is not tolerated. It is possible in principle (two
/// transitions inside one render) and vanishingly rare in practice, and a page
/// that shrugged at arbitrary drift could never catch the torn scrape this
/// check exists for.
fn reconcile_state(
    gauge: BreakerState,
    implied: BreakerState,
) -> Result<Option<BreakerState>, String> {
    if gauge == implied {
        return Ok(None);
    }
    let one_step_ahead = BreakerState::TRANSITIONS
        .iter()
        .any(|(from, to)| *from == gauge && *to == implied);
    if one_step_ahead {
        return Ok(Some(implied));
    }
    // Every pair that is not one step *ahead* is one step behind: the four
    // transitions connect all three states pairwise, so there is no third
    // case to word. Behind is the impossible one — the counter is incremented
    // under the same lock that stores the state, so a scrape cannot show a
    // state whose own transition has not been counted yet.
    Err(format!(
        "{CIRCUIT_BREAKER_STATE} reads {} but the {BREAKER_TRANSITIONS_TOTAL} counts describe a \
         breaker that is {} — the counters are published under the same lock that stores the \
         state, so they can run one transition ahead of the gauge but never behind it. The gauge \
         and the counters are not describing the same breaker",
        gauge.as_str(),
        implied.as_str(),
    ))
}

/// The share each upstream actually served, summed across method and status
/// class. Read straight off the scrape rather than through [`MetricsView`]:
/// this family is not part of that view's contract, and coupling the two would
/// tie the rollout answer to a section that renders something else.
///
/// **An absent side is not a failure, and not a zero.** `limen_requests_total`
/// is registered on the first request of its kind, so a route serving 0% to new
/// legitimately carries no `new` series at all — the same shape a lost counter
/// takes. The scrape-level question ("did this come from a limen exporter?") is
/// already settled upstream by [`MetricsView`]'s required families, so nothing
/// is gained by failing here and a real 0%-stage rollout would be failed for
/// being at 0%. What the page owes instead is *rendering*: the side is carried
/// as `None` all the way to [`observed_cell`], which prints
/// "no series = zero recorded" beside the share rather than a bare percentage.
fn read_observed(scrape: &Scrape, route: &str) -> Result<Reading<ObservedShare>, String> {
    let samples = route_samples(scrape, REQUESTS_TOTAL, route);
    // `None` is "this side was never counted", which is not the same fact as a
    // side that was counted and summed to zero.
    let side = |upstream: Upstream| -> Result<Option<u64>, String> {
        let mut total = None::<u64>;
        for sample in samples
            .iter()
            .filter(|s| s.labels.get("upstream").map(String::as_str) == Some(upstream.as_str()))
        {
            let value = exact_u64(REQUESTS_TOTAL, &sample.raw_value)?;
            total = Some(total.unwrap_or(0).checked_add(value).ok_or_else(|| {
                format!(
                    "the {REQUESTS_TOTAL} counts for upstream {} do not fit a 64-bit sum",
                    upstream.as_str()
                )
            })?);
        }
        Ok(total)
    };
    Ok(Reading::Known(ObservedShare {
        new: side(Upstream::New)?,
        legacy: side(Upstream::Legacy)?,
    }))
}

/// The one series of a process-wide family, or why there is not exactly one.
fn sole_process_series<'a>(scrape: &'a Scrape, family: &'static str) -> Result<&'a Sample, String> {
    match scrape.family(family).collect::<Vec<_>>().as_slice() {
        [one] => Ok(one),
        [] => Err(format!(
            "the scrape carries no {family} series — limen's control plane sets the \
             flag-provider gauges on every scrape, so their absence means this scrape cannot say \
             whether the flags were fresh"
        )),
        many => Err(format!(
            "the scrape carries {} {family} series — one provider has one health, and \
             reconciling two readings would be inventing it",
            many.len()
        )),
    }
}

/// Whether the three gauges could have come from one `health()` snapshot.
///
/// They always do — `CachedFlags::health` derives all three from a single read
/// under one lock and the `/metrics` handler writes them together — so a tuple
/// that contradicts itself is corruption, a merge of two scrapes, or a document
/// somebody edited. The legal tuples, given `ttl = flags.stale_ttl_ms / 1000`
/// and `stale = age > ttl` (or no successful refresh at all):
///
/// | `stale` | `staleness_seconds` | legal | why                                    |
/// |---------|---------------------|-------|----------------------------------------|
/// | 1       | `-1` (never)        | yes   | never refreshed is always stale        |
/// | 1       | `age >= ttl`        | yes   | aged past the TTL                      |
/// | 0       | `age <= ttl`        | yes   | refreshed within the TTL               |
/// | 0       | `-1` (never)        | **no**| never refreshed cannot read fresh      |
/// | 0       | `age > ttl`         | **no**| past the TTL cannot read fresh         |
/// | 1       | `age < ttl`         | **no**| inside the TTL cannot read stale       |
///
/// The boundary (`age == ttl`) is legal on both sides deliberately: the gauge
/// is a millisecond count divided into seconds and the comparison is strict, so
/// refusing equality would fail a healthy provider over a rounding.
///
/// Without a config there is no TTL to check against, and only the
/// TTL-independent row — fresh with no successful refresh, ever — can be
/// judged. That one needs no TTL: it is a contradiction at any TTL.
fn flag_tuple_fault(truth: &FlagProviderTruth, stale_ttl_ms: Option<u64>) -> Option<String> {
    match (truth.stale, truth.staleness_seconds) {
        (false, None) => Some(format!(
            "{FLAG_PROVIDER_STALE} reads 0 (fresh) while {FLAG_STALENESS_SECONDS} reads -1 (no \
             successful refresh has ever happened) — a provider that never refreshed is stale at \
             every TTL, so these two gauges did not come from one health snapshot"
        )),
        (stale, Some(age)) => {
            let ttl = stale_ttl_ms? as f64 / 1000.0;
            match (stale, age) {
                (false, age) if age > ttl => Some(format!(
                    "{FLAG_PROVIDER_STALE} reads 0 (fresh) while {FLAG_STALENESS_SECONDS} reads \
                     {age} — past this config's stale_ttl_ms of {ttl}s, which is the very \
                     condition that sets the stale gauge"
                )),
                (true, age) if age < ttl => Some(format!(
                    "{FLAG_PROVIDER_STALE} reads 1 (stale) while {FLAG_STALENESS_SECONDS} reads \
                     {age} — inside this config's stale_ttl_ms of {ttl}s, so nothing in limen \
                     could have set the stale gauge"
                )),
                _ => None,
            }
        }
        (true, None) => None,
    }
}

/// The three process-wide flag-provider gauges, which limen's control plane
/// refreshes on *every* scrape — so their absence beside live rollout series is
/// a torn or foreign scrape, not a quiet provider.
fn read_flags(scrape: &Scrape, stale_ttl_ms: Option<u64>) -> FlagReading {
    let read = || -> Result<FlagProviderTruth, String> {
        let sole = |family: &'static str| sole_process_series(scrape, family);
        let raw_stale = &sole(FLAG_PROVIDER_STALE)?.raw_value;
        // A boolean written as a float: exactly 0 or exactly 1, and anything
        // between them is a reading this page will not round toward "fresh".
        let raw = finite(FLAG_PROVIDER_STALE, raw_stale)?;
        let stale = if raw == 0.0 {
            false
        } else if raw == 1.0 {
            true
        } else {
            return Err(format!(
                "{FLAG_PROVIDER_STALE} reads {raw_stale:?}, which is neither 0 (fresh) nor 1 \
                 (stale)"
            ));
        };
        let raw_age = &sole(FLAG_STALENESS_SECONDS)?.raw_value;
        let age = finite(FLAG_STALENESS_SECONDS, raw_age)?;
        let staleness_seconds = if age == -1.0 {
            // The exporter's sentinel for "no successful refresh, ever".
            None
        } else if age < 0.0 {
            return Err(format!(
                "{FLAG_STALENESS_SECONDS} reads {raw_age:?} — the only negative value this gauge \
                 carries is the -1 sentinel for a provider that has never refreshed"
            ));
        } else {
            Some(age)
        };
        Ok(FlagProviderTruth {
            stale,
            staleness_seconds,
            consecutive_failures: exact_u64(
                FLAG_CONSECUTIVE_FAILURES,
                &sole(FLAG_CONSECUTIVE_FAILURES)?.raw_value,
            )?,
        })
    };
    match read() {
        Ok(truth) => match flag_tuple_fault(&truth, stale_ttl_ms) {
            Some(why) => FlagReading::Contradiction(why),
            None => FlagReading::Known(truth),
        },
        Err(why) => FlagReading::Absent(why),
    }
}

/// A reading whose failure rejects the whole row: the error becomes the row's
/// rejection, and the cell points at it rather than repeating it — the row
/// renders as one spanning unavailable, so a per-cell copy would never be read.
fn or_reject<T>(result: Result<Reading<T>, String>, rejected: &mut Vec<String>) -> Reading<T> {
    match result {
        Ok(reading) => reading,
        Err(why) => {
            rejected.push(why);
            Reading::Unknown("see the row's rejection above".to_string())
        }
    }
}

/// One route's row, checked against the config that declared it.
fn rollout_row(scrape: &Scrape, route: &ConfigRoute, flags: &FlagReading) -> RolloutRow {
    let mut rejected = Vec::new();

    // Each cell is gated on the config first: a reading the config rules out is
    // not applicable, never a series the scrape failed to carry.
    let target = if route.expects_target_series() {
        or_reject(
            read_target(scrape, &route.id).map(Reading::Known),
            &mut rejected,
        )
    } else {
        Reading::NotApplicable("only a percentage_split route resolves a rollout target")
    };

    let observed = or_reject(read_observed(scrape, &route.id), &mut rejected);

    let gauge_state = if route.breaker.enabled {
        or_reject(
            read_breaker_state(scrape, &route.id).map(Reading::Known),
            &mut rejected,
        )
    } else {
        Reading::NotApplicable("no circuit breaker is configured on this route")
    };

    let transitions = if route.breaker_consulted() {
        or_reject(
            read_transitions(scrape, &route.id).map(Reading::Known),
            &mut rejected,
        )
    } else {
        Reading::NotApplicable("this route never consults a circuit breaker")
    };

    // The gauge against the history: two readings of one breaker, taken
    // microseconds apart by the same handler. A one-step skew is the race
    // between them and renders as one; anything else means they are not
    // readings of the same breaker at all.
    let (breaker_state, state_skew) = match (&gauge_state, &transitions) {
        (Reading::Known(gauge), Reading::Known(counts)) => {
            match state_from_transitions(*counts)
                .and_then(|implied| reconcile_state(*gauge, implied).map(|skew| (implied, skew)))
            {
                // Under a skew the *more diverting* of the two readings is the
                // one reported: a breaker caught mid-transition is not licence
                // to report the calmer end of it.
                Ok((implied, Some(_))) => {
                    let worst = [*gauge, implied]
                        .into_iter()
                        .max_by_key(|state| match state {
                            BreakerState::Closed => 0,
                            BreakerState::HalfOpen => 1,
                            BreakerState::Open => 2,
                        })
                        .unwrap_or(*gauge);
                    (Reading::Known(worst), Some(implied))
                }
                Ok((_, None)) => (Reading::Known(*gauge), None),
                Err(why) => {
                    rejected.push(why);
                    (
                        Reading::Unknown("see the row's rejection above".to_string()),
                        None,
                    )
                }
            }
        }
        _ => (gauge_state, None),
    };

    // The one cross-family check: a stale provider displaces the rollout
    // outright (`resolve_percentage` returns the fail-safe before it looks at
    // any flag), so a nonzero target beside `stale 1` is a pair of readings no
    // limen produces — and exactly the pair that would let a displaced rollout
    // render as a running one.
    if let (Some(flags), Some(target)) = (flags.known(), target.known()) {
        if flags.stale && *target != 0.0 {
            rejected.push(format!(
                "the flag provider is stale, so this route's resolved target must be 0 (fail-safe \
                 displaces the rollout), but {ROLLOUT_RESOLVED_TARGET_PERCENTAGE} reads {target}"
            ));
        }
    }

    RolloutRow {
        id: route.id.clone(),
        mode: route.mode,
        failover_safe: route.failover_safe,
        rollout: route.rollout.clone(),
        breaker: route.breaker.clone(),
        breaker_consulted: route.breaker_consulted(),
        rejected,
        truth: RouteTruth {
            target,
            observed,
            breaker_state,
            transitions,
            state_skew,
        },
    }
}

/// Rollout series the config cannot account for.
///
/// Two shapes, both meaning the scrape and the config describe different
/// deployments: a `route` label the config has never heard of, and a series on
/// a configured route that cannot own one — a target on a route that is not a
/// `percentage_split`, transitions on a route whose breaker is never consulted,
/// a breaker state on a route with no breaker. `register_rollout_series` emits
/// exactly the set the config implies, so anything else came from a different
/// route table, and a page that reported the rows it recognized while
/// swallowing the rest would be describing half a deployment.
fn stray_series(scrape: &Scrape, config: &ConfigView) -> Vec<String> {
    /// Whether a configured route may own a series of this family — the
    /// config-side mirror of what `register_rollout_series` emits.
    type MayOwn = fn(&ConfigRoute) -> bool;

    let owners: [(&str, MayOwn); 3] = [
        (ROLLOUT_RESOLVED_TARGET_PERCENTAGE, |r| {
            r.expects_target_series()
        }),
        (BREAKER_TRANSITIONS_TOTAL, ConfigRoute::breaker_consulted),
        (CIRCUIT_BREAKER_STATE, |r| r.breaker.enabled),
    ];
    let mut out = Vec::new();
    for (family, may_own) in owners {
        let mut unknown = BTreeSet::new();
        let mut unowned = BTreeSet::new();
        for sample in scrape.family(family) {
            let Some(id) = sample.labels.get("route") else {
                continue;
            };
            match config.route(id) {
                None => unknown.insert(id.clone()),
                Some(route) if !may_own(route) => unowned.insert(id.clone()),
                Some(_) => false,
            };
        }
        for id in unknown {
            out.push(format!(
                "the scrape carries a {family} series for route {id}, which this config does not \
                 declare — the scrape and the config describe different route tables"
            ));
        }
        for id in unowned {
            out.push(format!(
                "the scrape carries a {family} series for route {id}, which this config gives no \
                 way to produce one — limen registers that series only for the routes that can \
                 emit it"
            ));
        }
    }
    out
}

/// Build the section's model from the scrape the counters section already
/// parsed, checked against the config's rollout routes.
///
/// `Err` is reserved for the one thing that makes the whole section
/// unreadable: a scrape that carries no rollout family at all while the config
/// says there was a rollout to report on.
fn rollout_view(
    scrape: &Scrape,
    config: Option<&ConfigView>,
) -> Result<RolloutResilienceView, String> {
    let Some(config) = config else {
        // No config: the route set the scrape should carry cannot be known, so
        // nothing per-route may be required of it. The provider gauges are
        // process-wide and readable regardless.
        return Ok(RolloutResilienceView {
            scope: RolloutScope::Unchecked,
            // No config, so no TTL to check the tuple against: only the
            // contradiction that holds at every TTL can be caught here.
            flags: read_flags(scrape, None),
            rows: Vec::new(),
            fail_safe_mode: None,
            stray: Vec::new(),
        });
    };
    if config.rollout_routes().next().is_none() {
        return Ok(RolloutResilienceView::none_configured());
    }
    if !ROLLOUT_FAMILIES.iter().any(|f| scrape.has_family(f)) {
        return Err(format!(
            "this config declares rollout routes but the scrape carries no rollout truth at all: \
             none of {} is present. The limen that produced it predates these series (or the \
             scrape came from something else) — either way it cannot say what the rollout was \
             doing, which is not the same as a rollout at 0%",
            ROLLOUT_FAMILIES.join(", ")
        ));
    }
    let flags = read_flags(scrape, Some(config.stale_ttl_ms));
    let rows = config
        .rollout_routes()
        .map(|route| rollout_row(scrape, route, &flags))
        .collect();
    Ok(RolloutResilienceView {
        scope: RolloutScope::Configured,
        flags,
        rows,
        fail_safe_mode: Some(config.fail_safe_mode),
        stray: stray_series(scrape, config),
    })
}

/// The sink's records, split the way `verdict::evaluate` splits them.
///
/// The reserved `__` namespace is **not** part of the mismatch answer: the
/// canary is a record limen writes on purpose to prove the record→flush→report
/// pipeline works, and counting it as a mismatch turns the very evidence that
/// the sink is healthy into a reason to call the run dirty. `limen verdict`
/// subtracts the whole reserved namespace from `mismatches_total` and omits it
/// from `sink_mismatches_by_route`, reporting the canary separately as
/// `canary_records`; this mirrors that split exactly.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SinkCounts {
    /// Records outside the reserved namespace: the mismatch answer.
    pub mismatches: usize,
    /// Records under [`CANARY_ROUTE_ID`].
    pub canary: usize,
    /// Records under some *other* reserved id, by id. Nothing limen writes
    /// lands here, and `verdict`'s per-route reconciliation fails on one (no
    /// counter can match it), so the page treats it the same way.
    pub other_reserved: BTreeMap<String, usize>,
}

impl SinkCounts {
    fn from_report(report: &Report) -> SinkCounts {
        let mut counts = SinkCounts::default();
        for route in &report.routes {
            if route.route_id == CANARY_ROUTE_ID {
                counts.canary += route.count;
            } else if route.route_id.starts_with(RESERVED_ROUTE_ID_PREFIX) {
                *counts
                    .other_reserved
                    .entry(route.route_id.clone())
                    .or_default() += route.count;
            } else {
                counts.mismatches += route.count;
            }
        }
        counts
    }
}

/// Whether a route id belongs to limen's internal reserved namespace.
fn is_reserved(route_id: &str) -> bool {
    route_id.starts_with(RESERVED_ROUTE_ID_PREFIX)
}

/// How the sink directory reconciles — the one input whose *absence of
/// content* is most easily misread as cleanliness.
#[derive(Debug, Clone, PartialEq)]
pub enum SinkState {
    /// The directory could not be read.
    Unavailable(String),
    /// Readable, but holds no sink files at all. Sink files are created on the
    /// first mismatch, so this is indistinguishable from a pipeline that never
    /// ran: never clean.
    NoFiles,
    /// Files exist, hold zero mismatches, and a clean verdict whose
    /// `sink_integrity` check passed vouches for the pipeline behind them.
    VerifiedZero,
    /// Files exist and hold zero mismatches, but nothing vouches for them.
    UnverifiedZero,
    /// Files exist and hold mismatches.
    Mismatches(usize),
}

impl SinkState {
    fn word(&self) -> &'static str {
        match self {
            SinkState::Unavailable(_) => "UNAVAILABLE",
            SinkState::NoFiles => "NO SINK FILES",
            SinkState::VerifiedZero => "ZERO MISMATCHES (VOUCHED FOR)",
            SinkState::UnverifiedZero => "ZERO MISMATCHES (UNVERIFIED)",
            SinkState::Mismatches(_) => "MISMATCHES RECORDED",
        }
    }

    fn class(&self) -> &'static str {
        match self {
            SinkState::Unavailable(_) | SinkState::NoFiles | SinkState::UnverifiedZero => "warn",
            SinkState::VerifiedZero => "good",
            SinkState::Mismatches(_) => "bad",
        }
    }

    /// What the state means, in the words the mismatches section prints.
    fn prose(&self) -> &str {
        match self {
            SinkState::Unavailable(why) => why,
            SinkState::NoFiles => {
                "A sink file is created by the first record written to it, so an empty \
                 directory is indistinguishable from a pipeline that never ran — or one that \
                 cannot write at all. A verdict run with --canary leaves a record and settles \
                 the question."
            }
            SinkState::VerifiedZero => {
                "Zero mismatch records across the files read, vouched for by a clean online \
                 verdict whose sink-integrity check passed."
            }
            SinkState::UnverifiedZero => {
                "Zero mismatch records across the files read, but nothing vouches for the \
                 pipeline that wrote them."
            }
            SinkState::Mismatches(_) => "Mismatch records are on disk.",
        }
    }
}

/// How one route stands against the verdict's floors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloorClass {
    /// A floors row exists and is met.
    Met,
    /// A floors row exists and is not met.
    Unmet,
    /// The config does not expect a floors row (comparison disabled, or an
    /// explicit floor of 0).
    NotApplicable,
    /// The config expects a floors row and the verdict does not carry one.
    MissingUnexpectedly,
    /// The route appears in an artifact but not in the config.
    UnknownRoute,
    /// The config or the verdict is absent, so nothing can be classified.
    Undetermined,
}

impl FloorClass {
    fn word(self) -> &'static str {
        match self {
            FloorClass::Met => "MET",
            FloorClass::Unmet => "UNMET",
            FloorClass::NotApplicable => "NOT APPLICABLE",
            FloorClass::MissingUnexpectedly => "MISSING UNEXPECTEDLY",
            FloorClass::UnknownRoute => "UNKNOWN ROUTE",
            FloorClass::Undetermined => "UNDETERMINED",
        }
    }

    fn class(self) -> &'static str {
        match self {
            FloorClass::Met => "good",
            FloorClass::Unmet | FloorClass::MissingUnexpectedly | FloorClass::UnknownRoute => "bad",
            FloorClass::NotApplicable => "neutral",
            FloorClass::Undetermined => "warn",
        }
    }
}

/// One row of the union join across every artifact.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteRow {
    pub id: String,
    pub in_config: bool,
    pub in_verdict: bool,
    pub in_sink: bool,
    pub in_profile: bool,
    pub in_metrics: bool,
    pub floor_class: FloorClass,
    pub comparisons: Option<u64>,
    pub floor: Option<u64>,
}

/// Where the banner landed, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerState {
    Clean,
    Incomplete,
    Failure,
}

impl BannerState {
    /// The headline. Uppercase `CLEAN` appears nowhere on this page but the
    /// clean banner — which is why the incomplete headline says "PASSING"
    /// rather than the "NOT A CLEAN RESULT" it once did: the phrase read well
    /// and quietly broke the contract, since "does this page claim success?"
    /// is a substring search and the denial contained the claim.
    fn headline(self) -> &'static str {
        match self {
            BannerState::Clean => "CLEAN",
            BannerState::Incomplete => "INCOMPLETE — NOT A PASSING RESULT",
            BannerState::Failure => "FAILURE",
        }
    }

    fn class(self) -> &'static str {
        match self {
            BannerState::Clean => "good",
            BannerState::Incomplete => "warn",
            BannerState::Failure => "bad",
        }
    }

    /// The sentence under the headline: what this state licenses the reader to
    /// conclude, which for two of the three is "nothing".
    fn note(self) -> &'static str {
        match self {
            BannerState::Clean => {
                "Every required input was present and parsed, every artifact agreed, and the \
                 sink reconciles."
            }
            BannerState::Incomplete => {
                "No failure was found, but a required input is missing — this page cannot \
                 vouch for the run."
            }
            BannerState::Failure => {
                "At least one signal below is a failure. Nothing on this page may be read as \
                 a passing run."
            }
        }
    }
}

/// The banner's verdict on the page as a whole.
#[derive(Debug, Clone, PartialEq)]
pub struct Banner {
    pub state: BannerState,
    /// Signals that force FAILURE.
    pub failures: Vec<String>,
    /// Required inputs that were absent (no failure signal, but nothing green
    /// can be claimed either).
    pub incomplete: Vec<String>,
    /// Optional inputs that were not provided — neutral, listed so the reader
    /// knows what the page did *not* look at.
    pub notes: Vec<String>,
}

/// Every input as the page read it, plus what checking them against each other
/// turned up. This is the whole of what the banner decides from.
#[derive(Debug, Clone)]
pub struct Evidence {
    pub sink: Section<Report>,
    /// The sink's records split into mismatches, canary and other reserved —
    /// all zero when the directory could not be read.
    pub sink_counts: SinkCounts,
    pub sink_state: SinkState,
    pub config: Section<ConfigView>,
    pub verdict: Section<VerdictArtifact>,
    pub profile: Section<ProfileDto>,
    pub metrics: Section<MetricsView>,
    /// A second reading of the metrics artifact: what the rollout targeted,
    /// what it served, and what its breakers and flag provider were doing.
    pub rollout: Section<RolloutResilienceView>,
    /// Semantic violations found in a full verdict document.
    pub verdict_violations: Vec<String>,
    /// Cross-artifact disagreements.
    pub drift: Vec<String>,
}

impl Evidence {
    /// The verdict, when one parsed as a full report.
    fn full_verdict(&self) -> Option<&VerdictDto> {
        self.verdict.get().and_then(VerdictArtifact::full)
    }
}

/// Everything the renderer needs: the parsed inputs, the joins over them, and
/// the banner they add up to.
#[derive(Debug, Clone)]
pub struct PageModel {
    pub inputs: Inputs,
    pub evidence: Evidence,
    pub routes: Vec<RouteRow>,
    pub banner: Banner,
}

// ---------------------------------------------------------------------------
// Gathering
//
// Nothing in this section can fail the process: an unreadable artifact becomes
// a `Section::Unavailable` the page renders and the banner reacts to.
// ---------------------------------------------------------------------------

fn read_sink(dir: &Path) -> Section<Report> {
    match sink::read_report(dir, &ReportFilter::default(), REPORT_EXAMPLES_PER_ROUTE) {
        Ok(report) => Section::Ok(report),
        Err(e) => Section::Unavailable(format!("cannot read sink directory: {e}")),
    }
}

fn read_config(path: Option<&PathBuf>) -> Section<ConfigView> {
    let Some(path) = path else {
        return Section::NotProvided;
    };
    // Deliberately *without* the environment layer: the page describes the
    // config file it was handed, not that file as this shell would override it.
    match load_config(path, &ConfigOverrides::default()) {
        Ok(loaded) => Section::Ok(ConfigView::from_config(&loaded.config)),
        Err(e) => Section::Unavailable(format!("cannot load config: {e}")),
    }
}

fn read_verdict(path: Option<&PathBuf>) -> Section<VerdictArtifact> {
    let Some(path) = path else {
        return Section::NotProvided;
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => return Section::Unavailable(format!("cannot read verdict artifact: {e}")),
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(e) => return Section::Unavailable(format!("verdict artifact is not JSON: {e}")),
    };
    // The exit-50 shape first, on its discriminator. It is not a degenerate
    // verdict, it is the absence of one.
    if value.get("mode").and_then(|m| m.as_str()) == Some("unavailable") {
        return match serde_json::from_value::<UnavailableDto>(value) {
            Ok(dto) => Section::Ok(VerdictArtifact::InputUnavailable(dto)),
            Err(e) => Section::Unavailable(format!(
                "verdict artifact declares mode \"unavailable\" but does not match that \
                 shape: {e}"
            )),
        };
    }
    match serde_json::from_value::<VerdictDto>(value) {
        Ok(dto) => Section::Ok(VerdictArtifact::Full(Box::new(dto))),
        Err(e) => Section::Unavailable(format!("verdict artifact is not a verdict document: {e}")),
    }
}

fn read_profile(path: Option<&PathBuf>) -> Section<ProfileDto> {
    let Some(path) = path else {
        return Section::NotProvided;
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => return Section::Unavailable(format!("cannot read profile artifact: {e}")),
    };
    match serde_json::from_str::<ProfileDto>(&text) {
        Ok(dto) => Section::Ok(dto),
        Err(e) => Section::Unavailable(format!("profile artifact is not an observe profile: {e}")),
    }
}

/// One metrics artifact, read once. The scrape is kept beside the counters
/// view so the rollout section can be a second reading of the *same* parse —
/// two readers of one document cannot disagree about what the document said.
struct MetricsRead {
    view: Section<MetricsView>,
    scrape: Option<Scrape>,
}

fn read_metrics(path: Option<&PathBuf>) -> MetricsRead {
    let unavailable = |why: String| MetricsRead {
        view: Section::Unavailable(why),
        scrape: None,
    };
    let Some(path) = path else {
        return MetricsRead {
            view: Section::NotProvided,
            scrape: None,
        };
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => return unavailable(format!("cannot read metrics artifact: {e}")),
    };
    let scrape = match Scrape::parse(&text) {
        Ok(scrape) => scrape,
        Err(e) => return unavailable(format!("metrics artifact is not a scrape: {e}")),
    };
    match MetricsView::from_scrape(&scrape) {
        Ok(view) => MetricsRead {
            view: Section::Ok(view),
            scrape: Some(scrape),
        },
        Err(e) => unavailable(e),
    }
}

/// The rollout section's standing, decided in the order the three-way
/// semantics require.
///
/// The honest empty comes **first**: a config that declares no rollout route
/// has no rollout to report whatever the scrape turned out to be, and calling
/// that section unavailable would invent a subject for it.
///
/// After that the section follows the metrics artifact, mirroring
/// `render_counters`' contract: absent `--metrics` is NOT PROVIDED (an absence
/// of evidence, not a rollout at zero), and a metrics artifact this page
/// refused is UNAVAILABLE here too — a scrape limen's own gate will not read is
/// not one this section will mine rollout truth out of.
fn read_rollout(
    metrics: &MetricsRead,
    config: Option<&ConfigView>,
) -> Section<RolloutResilienceView> {
    if let Some(config) = config {
        if config.rollout_routes().next().is_none() {
            return Section::Ok(RolloutResilienceView::none_configured());
        }
    }
    match (&metrics.view, &metrics.scrape) {
        (Section::NotProvided, _) => Section::NotProvided,
        (Section::Unavailable(why), _) => Section::Unavailable(format!(
            "the metrics artifact could not be read, so nothing about the rollout could be: {why}"
        )),
        (Section::Ok(_), Some(scrape)) => match rollout_view(scrape, config) {
            Ok(view) => Section::Ok(view),
            Err(why) => Section::Unavailable(why),
        },
        // Unreachable: a parsed counters view is built from a scrape this read
        // kept. Stated as an unavailable rather than an unwrap, because the one
        // thing this page may never do is render a missing input as a zero.
        (Section::Ok(_), None) => {
            Section::Unavailable("the parsed scrape was not retained".to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Semantics and drift
// ---------------------------------------------------------------------------

/// The exit code a verdict name is required to carry.
fn expected_exit(name: &str) -> Option<u8> {
    match name {
        "clean" => Some(0),
        "mismatches-found" => Some(10),
        "floors-unmet" => Some(20),
        "sink-integrity-failure" => Some(30),
        "drain-timeout" => Some(40),
        _ => None,
    }
}

/// Check a full verdict document against itself. A verdict whose parts
/// contradict each other describes no run at all — most importantly, an
/// `exit_code: 0` that its own checks do not support is exactly the shape a
/// hand-edited or half-written artifact takes.
fn semantic_violations(v: &VerdictDto) -> Vec<String> {
    let mut out = Vec::new();

    match expected_exit(&v.verdict) {
        Some(expected) if expected != v.exit_code => out.push(format!(
            "verdict {:?} carries exit_code {} (expected {expected})",
            v.verdict, v.exit_code
        )),
        None if v.exit_code == 0 => out.push(format!(
            "exit_code 0 under the unrecognized verdict name {:?} — only \"clean\" may exit 0",
            v.verdict
        )),
        _ => {}
    }

    if v.exit_code == 0 {
        for (name, check) in v.checks.named() {
            if check.is_fail() {
                out.push(format!("exit_code 0 with a failed {name} check"));
            }
        }
        if v.mismatches_total != 0 {
            out.push(format!(
                "exit_code 0 with {} mismatch(es) counted",
                v.mismatches_total
            ));
        }
        if v.mode != "online" && v.mode != "offline" {
            out.push(format!(
                "exit_code 0 under the unrecognized mode {:?} — a verdict is taken online or \
                 offline, and an unrecognized mode is not a mode that checked anything",
                v.mode
            ));
        }
        // A clean *online* verdict is a positive claim about all five checks,
        // so every one of them has to have actually run. Absent this, a check
        // reporting nothing at all — or a word neither this page nor the
        // verdict recognizes — reads as "not a failure" and rides through.
        // (Offline is exempt only because the banner already refuses it
        // outright: its checks are skipped by construction.)
        if v.mode != "offline" {
            for (name, check) in v.checks.named() {
                if check.is_fail() {
                    continue; // already reported above
                }
                // The canary is the one check a clean run may legitimately
                // skip: it only runs when `--canary` was asked for — and
                // `evaluate_canary` emits `skipped` exactly when zero canary
                // records were counted, so a skip alongside a nonzero
                // `canary_records` (or a pass alongside zero) is a state no
                // real verdict produces. Only a torn or edited artifact can
                // carry it, and it must not ride through as clean.
                let acceptable = check.is_pass()
                    || (name == "canary" && check.is_skipped() && v.canary_records == 0);
                if !acceptable {
                    out.push(format!(
                        "exit_code 0 with the {name} check reporting {:?} — a clean run \
                         requires it to have passed",
                        check.status
                    ));
                }
                if name == "canary" && check.is_pass() && v.canary_records == 0 {
                    out.push(
                        "the canary check passed with canary_records 0 — a canary pass \
                         requires at least one counted record"
                            .to_string(),
                    );
                }
            }
        }
    }

    for row in &v.floors {
        if row.met != (row.comparisons >= row.floor) {
            out.push(format!(
                "floors row for route {:?} claims met={} with {} comparison(s) against a floor \
                 of {}",
                row.route_id, row.met, row.comparisons, row.floor
            ));
        }
    }

    let any_unmet = v.floors.iter().any(|r| !r.met);
    if any_unmet && !v.checks.floors.is_fail() {
        out.push(format!(
            "a floors row is unmet but the floors check reports {:?}",
            v.checks.floors.status
        ));
    }
    if !any_unmet && !v.floors.is_empty() && v.checks.floors.is_fail() {
        out.push("the floors check failed with every floors row met".to_string());
    }

    out
}

/// Cross-artifact reconciliation. Each finding names what disagreed: two
/// artifacts that contradict each other cannot both describe this campaign, so
/// neither may be believed.
fn drift_findings(
    config: Option<&ConfigView>,
    verdict: Option<&VerdictDto>,
    sink: Option<&Report>,
    profile: Option<&ProfileDto>,
    metrics: Option<&MetricsView>,
) -> Vec<String> {
    let mut out = Vec::new();

    // The canary the verdict counted against the canary records on disk. The
    // canary is the one record limen writes on purpose, so a disagreement here
    // is the pipeline itself failing to record — real signal, not bookkeeping.
    if let (Some(verdict), Some(sink)) = (verdict, sink) {
        let counts = SinkCounts::from_report(sink);
        if counts.canary as u64 != verdict.canary_records {
            out.push(format!(
                "the sink directory holds {} canary record(s) but the verdict counted {} — \
                 the record it wrote to prove the pipeline works is not the one on disk",
                counts.canary, verdict.canary_records
            ));
        }
    }

    // The sink on disk against the verdict's per-route map. The verdict
    // excludes the reserved namespace from that map, so the sink side does too.
    if let (Some(verdict), Some(sink)) = (verdict, sink) {
        let sunk: BTreeMap<&str, u64> = sink
            .routes
            .iter()
            .filter(|r| !is_reserved(&r.route_id))
            .map(|r| (r.route_id.as_str(), r.count as u64))
            .collect();
        let mut ids: BTreeSet<&str> = sunk.keys().copied().collect();
        ids.extend(verdict.sink_mismatches_by_route.keys().map(String::as_str));
        for id in ids {
            let here = sunk.get(id).copied().unwrap_or(0);
            let there = verdict
                .sink_mismatches_by_route
                .get(id)
                .copied()
                .unwrap_or(0);
            if here != there {
                out.push(format!(
                    "route {id}: the sink directory holds {here} mismatch(es) but the verdict \
                     recorded {there}"
                ));
            }
        }
    }

    // The verdict's floors against the floors this config declares.
    if let (Some(config), Some(verdict)) = (config, verdict) {
        for row in &verdict.floors {
            let Some(route) = config.route(&row.route_id) else {
                // Not in the config at all — reported as an unknown route id
                // below, where every artifact is checked the same way.
                continue;
            };
            if !route.expects_floor_row {
                // `evaluate_floors` only ever emits rows for routes that are
                // comparison-enabled with a non-zero floor, so a row here means
                // the two documents disagree about what this route even is.
                out.push(format!(
                    "route {}: the verdict carries a floors row for it, but this config \
                     neither compares nor floors it",
                    row.route_id
                ));
            } else if route.floor != row.floor {
                out.push(format!(
                    "route {}: the verdict floored at {} but this config declares {}",
                    row.route_id, row.floor, route.floor
                ));
            }
        }
        for route in config.routes.iter().filter(|r| r.expects_floor_row) {
            if !verdict.floors.iter().any(|f| f.route_id == route.id) {
                out.push(format!(
                    "route {}: comparison-enabled and floored at {}, but the verdict carries no \
                     floors row for it",
                    route.id, route.floor
                ));
            }
        }
    }

    // Route ids in an artifact that this config has never heard of: a config
    // edited after the run, or artifacts from a different deployment.
    if let Some(config) = config {
        let known: BTreeSet<&str> = config.routes.iter().map(|r| r.id.as_str()).collect();
        let mut seen: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
        let mut note = |id: &str, source: &'static str| {
            // The reserved namespace is limen's own — a config never declares
            // it, so its absence there is not drift.
            if is_reserved(id) || known.contains(id) {
                return;
            }
            seen.entry(id.to_string()).or_default().insert(source);
        };
        if let Some(verdict) = verdict {
            for row in &verdict.floors {
                note(&row.route_id, "verdict floors");
            }
            for id in verdict.sink_mismatches_by_route.keys() {
                note(id, "verdict sink counts");
            }
        }
        if let Some(sink) = sink {
            for route in &sink.routes {
                note(&route.route_id, "sink");
            }
        }
        if let Some(profile) = profile {
            for id in profile.routes.keys() {
                note(id, "profile");
            }
        }
        if let Some(metrics) = metrics {
            for id in metrics.route_ids() {
                note(&id, "metrics");
            }
        }
        for (id, sources) in seen {
            out.push(format!(
                "route {id} appears in {} but not in this config",
                sources.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
    }

    out
}

/// The union join across every artifact, classified per route.
fn join_routes(
    config: Option<&ConfigView>,
    verdict: Option<&VerdictDto>,
    sink: Option<&Report>,
    profile: Option<&ProfileDto>,
    metrics: Option<&MetricsView>,
) -> Vec<RouteRow> {
    // Built once: the id set is needed for the union below *and* for every
    // row's metrics column.
    let metric_ids = metrics.map(MetricsView::route_ids).unwrap_or_default();

    let mut ids: BTreeSet<String> = BTreeSet::new();
    if let Some(config) = config {
        ids.extend(config.routes.iter().map(|r| r.id.clone()));
    }
    if let Some(verdict) = verdict {
        ids.extend(verdict.floors.iter().map(|f| f.route_id.clone()));
        ids.extend(verdict.sink_mismatches_by_route.keys().cloned());
    }
    if let Some(sink) = sink {
        ids.extend(sink.routes.iter().map(|r| r.route_id.clone()));
    }
    if let Some(profile) = profile {
        ids.extend(profile.routes.keys().cloned());
    }
    ids.extend(metric_ids.iter().cloned());
    // Reserved ids are limen's own internal records, not routes: no config
    // declares one, no floor applies to one, and every column of this table
    // would read "unknown". The canary has its own line in the mismatches
    // section, where it is evidence rather than an anomaly.
    ids.retain(|id| !is_reserved(id));

    ids.into_iter()
        .map(|id| {
            let configured = config.and_then(|c| c.route(&id));
            let row = verdict.and_then(|v| v.floors.iter().find(|f| f.route_id == id));
            let floor_class = match (config, verdict) {
                (None, _) | (_, None) => FloorClass::Undetermined,
                (Some(_), Some(_)) => match (configured, row) {
                    // A floors row for a route the config does not declare is
                    // still rendered on its merits: unmet is red either way.
                    (None, Some(row)) if !row.met => FloorClass::Unmet,
                    (None, _) => FloorClass::UnknownRoute,
                    (Some(_), Some(row)) if row.met => FloorClass::Met,
                    (Some(_), Some(_)) => FloorClass::Unmet,
                    (Some(c), None) if c.expects_floor_row => FloorClass::MissingUnexpectedly,
                    (Some(_), None) => FloorClass::NotApplicable,
                },
            };
            RouteRow {
                in_config: configured.is_some(),
                in_verdict: row.is_some()
                    || verdict.is_some_and(|v| v.sink_mismatches_by_route.contains_key(&id)),
                in_sink: sink.is_some_and(|s| s.routes.iter().any(|r| r.route_id == id)),
                in_profile: profile.is_some_and(|p| p.routes.contains_key(&id)),
                in_metrics: metric_ids.contains(&id),
                comparisons: row.map(|r| r.comparisons),
                // A route the config does not floor has no floor to show: its
                // `effective_min_comparisons()` is the default 1, and printing
                // that would read as a floor nothing ever asserted.
                floor: row
                    .map(|r| r.floor)
                    .or_else(|| configured.filter(|c| c.expects_floor_row).map(|c| c.floor)),
                floor_class,
                id,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The banner
// ---------------------------------------------------------------------------

/// Decide the banner from everything the page knows.
///
/// The three states are exhaustive and ordered: any failure signal wins; absent
/// that, any missing *required* input holds the page at INCOMPLETE; only a page
/// with every required input present, every provided input parsed, no drift and
/// a reconciling sink is CLEAN.
pub fn decide_banner(evidence: &Evidence) -> Banner {
    let Evidence {
        sink,
        sink_counts: _,
        sink_state,
        config,
        verdict,
        profile,
        metrics,
        rollout,
        verdict_violations,
        drift,
    } = evidence;
    let mut failures = Vec::new();
    let mut incomplete = Vec::new();
    let mut notes = Vec::new();

    match config {
        Section::NotProvided => incomplete
            .push("config: not provided (--config is required for a clean result)".to_string()),
        Section::Unavailable(why) => failures.push(format!("config: {why}")),
        // A config in which no route is both comparison-enabled and floored
        // makes every reconciliation on this page vacuous: there is no floors
        // row to miss, no coverage to fall short of, and a zero-mismatch sink
        // proves nothing about a pipeline that was never asked to compare
        // anything. `limen verdict` calls this exit 20 (floors-unmet) rather
        // than clean; the page takes the same position.
        Section::Ok(view) if !view.routes.iter().any(|r| r.expects_floor_row) => {
            failures.push(
                "config: floors nothing — no route is both comparison-enabled and floored, so \
                 a clean verdict over it would be vacuously green"
                    .to_string(),
            );
        }
        Section::Ok(_) => {}
    }

    match verdict {
        Section::NotProvided => incomplete
            .push("verdict: not provided (--verdict is required for a clean result)".to_string()),
        Section::Unavailable(why) => failures.push(format!("verdict: {why}")),
        Section::Ok(VerdictArtifact::InputUnavailable(dto)) => failures.push(format!(
            "verdict: input-unavailable (exit {}) — a tooling failure, never zero mismatches: {}",
            dto.exit_code, dto.error
        )),
        Section::Ok(VerdictArtifact::Full(v)) => {
            if v.exit_code != 0 {
                failures.push(format!(
                    "verdict: {} (exit {})",
                    if v.verdict.is_empty() {
                        "unnamed"
                    } else {
                        &v.verdict
                    },
                    v.exit_code
                ));
            }
            if v.mode == "offline" {
                failures.push(
                    "verdict: recorded in offline mode — drain, floors, sink integrity and \
                     canary were never checked, so this cannot be a clean result"
                        .to_string(),
                );
            }
            for row in v.floors.iter().filter(|r| !r.met) {
                failures.push(format!(
                    "floor unmet: route {} compared {} time(s) against a floor of {}",
                    row.route_id, row.comparisons, row.floor
                ));
            }
        }
    }

    for violation in verdict_violations {
        failures.push(format!("inconsistent verdict artifact: {violation}"));
    }
    for finding in drift {
        failures.push(format!("drift: {finding}"));
    }

    match profile {
        Section::NotProvided => notes.push("profile: not provided".to_string()),
        Section::Unavailable(why) => failures.push(format!("profile: {why}")),
        Section::Ok(_) => {}
    }
    match metrics {
        Section::NotProvided => notes.push("metrics: not provided".to_string()),
        Section::Unavailable(why) => failures.push(format!("metrics: {why}")),
        Section::Ok(_) => {}
    }
    // The rollout section is a reading of the metrics artifact, so it follows
    // that input's standing: not provided is a note the metrics row already
    // carries, and unreadable is a failure. What it adds is the per-route
    // truth — a route whose series are missing, duplicated or impossible, and
    // a flag provider that was stale while a rollout was supposed to be
    // running, are failures the counters section cannot see.
    match rollout {
        Section::NotProvided => {}
        Section::Unavailable(why) => failures.push(format!("rollout: {why}")),
        Section::Ok(view) => failures.extend(view.failures()),
    }

    if let Section::Ok(report) = sink {
        if report.malformed_lines > 0 {
            failures.push(format!(
                "sink: {} unparseable line(s) — the pipeline was interrupted mid-write, so \
                 records are missing",
                report.malformed_lines
            ));
        }
        // A reserved id limen does not write. `verdict`'s per-route
        // reconciliation fails on one too (no counter can ever match it), and
        // an id in limen's own namespace that limen did not put there is worth
        // the whole page.
        for (id, count) in &SinkCounts::from_report(report).other_reserved {
            failures.push(format!(
                "sink: {count} record(s) under the reserved route id {id} — the {RESERVED_ROUTE_ID_PREFIX} \
                 namespace is limen's own, and nothing limen writes uses that id"
            ));
        }
    }
    match sink_state {
        SinkState::Unavailable(why) => incomplete.push(format!("sink: {why}")),
        SinkState::NoFiles => incomplete.push(
            "sink: no sink files found — nothing has ever been recorded here, which is \
             indistinguishable from a sink that cannot write. Run the verdict with --canary: \
             the canary rides the real pipeline and leaves a record, which is what turns an \
             empty directory into evidence"
                .to_string(),
        ),
        SinkState::UnverifiedZero => incomplete.push(
            "sink: zero mismatches across the files read, but no clean verdict vouches for \
             the pipeline that wrote them"
                .to_string(),
        ),
        SinkState::Mismatches(n) => {
            failures.push(format!("sink: {n} mismatch record(s) on disk"));
        }
        SinkState::VerifiedZero => {}
    }

    let state = if !failures.is_empty() {
        BannerState::Failure
    } else if !incomplete.is_empty() {
        BannerState::Incomplete
    } else {
        BannerState::Clean
    };
    Banner {
        state,
        failures,
        incomplete,
        notes,
    }
}

/// Read every input and work out what the page says. Pure of process failure:
/// every unreadable input lands in the model as an unavailable section.
pub fn analyze(inputs: &Inputs) -> PageModel {
    let sink = read_sink(&inputs.sink_dir);
    let config = read_config(inputs.config.as_ref());
    let verdict = read_verdict(inputs.verdict.as_ref());
    let profile = read_profile(inputs.profile.as_ref());
    let metrics_read = read_metrics(inputs.metrics.as_ref());
    let rollout = read_rollout(&metrics_read, config.get());
    let metrics = metrics_read.view;

    let full_verdict = verdict.get().and_then(VerdictArtifact::full);
    let verdict_violations = full_verdict.map(semantic_violations).unwrap_or_default();

    // A verdict may only vouch for an empty sink when it is itself coherent,
    // online, clean, and its sink-integrity check actually passed.
    let vouched = full_verdict.is_some_and(|v| {
        verdict_violations.is_empty()
            && v.exit_code == 0
            && v.mode == "online"
            && v.checks.sink_integrity.is_pass()
    });
    // Mismatches, not records: a canary record is limen proving its own sink
    // works, and reading it as a mismatch would turn the evidence of a healthy
    // pipeline into a reason to call the run dirty.
    let sink_counts = sink.get().map(SinkCounts::from_report).unwrap_or_default();
    let sink_state = match &sink {
        Section::Unavailable(why) => SinkState::Unavailable(why.clone()),
        Section::NotProvided => SinkState::Unavailable("no sink directory".to_string()),
        Section::Ok(report) if report.files_read == 0 => SinkState::NoFiles,
        Section::Ok(_) if sink_counts.mismatches > 0 => {
            SinkState::Mismatches(sink_counts.mismatches)
        }
        Section::Ok(_) if vouched => SinkState::VerifiedZero,
        Section::Ok(_) => SinkState::UnverifiedZero,
    };

    let drift = drift_findings(
        config.get(),
        full_verdict,
        sink.get(),
        profile.get(),
        metrics.get(),
    );
    let routes = join_routes(
        config.get(),
        full_verdict,
        sink.get(),
        profile.get(),
        metrics.get(),
    );
    let evidence = Evidence {
        sink,
        sink_counts,
        sink_state,
        config,
        verdict,
        profile,
        metrics,
        rollout,
        verdict_violations,
        drift,
    };
    let banner = decide_banner(&evidence);

    PageModel {
        inputs: inputs.clone(),
        evidence,
        routes,
        banner,
    }
}

/// Read the inputs and render the page. Never fails: an unreadable artifact is
/// a section of the page, not an error.
pub fn render_report(inputs: &Inputs) -> String {
    render(&analyze(inputs))
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Escape a value for both text and attribute positions.
///
/// Every interpolated value on the page goes through this. Route ids, config
/// paths and verdict details all come from documents this tool did not write,
/// and a report that executes its input is worse than no report.
pub fn esc(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// A state pill: a colored chip whose text says the same thing the color does.
fn pill(class: &str, text: &str) -> String {
    format!("<span class=\"pill {class}\">{}</span>", esc(text))
}

/// A yes/no cell for the per-source presence columns.
fn present(yes: bool) -> String {
    if yes {
        pill("good", "yes")
    } else {
        pill("neutral", "no")
    }
}

/// A route-id cell. The id is repeated into a `title` so a long one stays
/// inspectable where the column truncates it — every table on the page keys on
/// route id, so they all draw this same cell.
fn route_cell(id: &str) -> String {
    let id = esc(id);
    format!("<td class=\"mono\" title=\"route id: {id}\">{id}</td>")
}

/// The same cell for a series that may carry no `route` label at all — the
/// pipeline counters are process-wide. An em dash, never an invented id.
fn route_label_cell(id: Option<&String>) -> String {
    match id {
        Some(id) => route_cell(id),
        None => "<td class=\"mono\" title=\"no route label\">—</td>".to_string(),
    }
}

/// A count, or an em dash where there is none to show. Never a zero: "the
/// verdict carried no floor for this route" and "the floor is 0" are different
/// claims and the page may not collapse them.
fn num(value: Option<u64>) -> String {
    value.map_or_else(|| "—".to_string(), |v| v.to_string())
}

/// A count map flattened into one cell: `GET 3, POST 1`. Escape at the call
/// site — the keys come from artifacts this tool did not write.
fn counts<'a, V: std::fmt::Display + 'a>(
    pairs: impl IntoIterator<Item = (&'a String, &'a V)>,
) -> String {
    pairs
        .into_iter()
        .map(|(key, n)| format!("{key} {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A titled list of findings, or nothing at all when there is nothing to list —
/// an empty heading would read as a heading that found nothing to say.
fn bullets(out: &mut String, title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    let _ = writeln!(
        out,
        "<p class=\"note\"><strong>{}</strong></p>\n<ul>",
        esc(title)
    );
    for item in items {
        let _ = writeln!(out, "<li>{}</li>", esc(item));
    }
    out.push_str("</ul>\n");
}

/// A section whose whole content is a state: a pill and the sentence behind it.
fn note(out: &mut String, class: &str, word: &str, text: &str) {
    let _ = writeln!(
        out,
        "<p class=\"note\">{} {}</p>",
        pill(class, word),
        esc(text)
    );
}

const STYLE: &str = "\
:root{color-scheme:light dark}
body{font:14px/1.5 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;\
margin:0;padding:2rem;background:#f6f7f9;color:#16191d}
main{max-width:70rem;margin:0 auto}
h1{font-size:1.4rem;margin:0 0 1rem}
h2{font-size:1.05rem;margin:2rem 0 .5rem;border-bottom:1px solid #d7dbe0;padding-bottom:.3rem}
h3{font-size:.95rem;margin:1.2rem 0 .4rem}
.banner{border-radius:8px;padding:1rem 1.2rem;border:2px solid;margin-bottom:1rem}
.banner .state{font-size:1.5rem;font-weight:700;letter-spacing:.04em}
.banner.good{background:#e6f6ea;border-color:#1c7a3c;color:#10431f}
.banner.warn{background:#fdf3df;border-color:#a06a06;color:#4d3403}
.banner.bad{background:#fdeaea;border-color:#a41b1b;color:#4d0f0f}
.banner ul{margin:.6rem 0 0;padding-left:1.2rem}
.banner li{margin:.15rem 0}
table{border-collapse:collapse;width:100%;background:#fff;border:1px solid #d7dbe0}
th,td{text-align:left;padding:.35rem .55rem;border-bottom:1px solid #e6e9ed;vertical-align:top}
th{background:#eef1f4;font-weight:600;font-size:.85rem}
td.num{text-align:right;font-variant-numeric:tabular-nums}
.mono{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:.85rem;\
word-break:break-all}
.pill{display:inline-block;padding:.05rem .45rem;border-radius:10px;font-size:.75rem;\
font-weight:600;border:1px solid}
.pill.good{background:#e6f6ea;border-color:#1c7a3c;color:#10431f}
.pill.warn{background:#fdf3df;border-color:#a06a06;color:#4d3403}
.pill.bad{background:#fdeaea;border-color:#a41b1b;color:#4d0f0f}
.pill.neutral{background:#eef1f4;border-color:#9aa3ad;color:#3b4249}
p.note{color:#4b535b;margin:.3rem 0}
footer{margin-top:2.5rem;padding-top:.8rem;border-top:1px solid #d7dbe0;color:#4b535b;\
font-size:.85rem}
";

/// Render the page. Deterministic and self-contained: one style block, no
/// scripts, and no reference to anything off this document.
pub fn render(model: &PageModel) -> String {
    let mut out = String::with_capacity(16 * 1024);
    out.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    out.push_str("<title>limen report</title>\n<style>\n");
    out.push_str(STYLE);
    out.push_str("</style>\n</head>\n<body>\n<main>\n");
    out.push_str("<h1>limen report</h1>\n");

    render_banner(&mut out, model);
    render_manifest(&mut out, model);
    render_checks(&mut out, model);
    render_routes(&mut out, model);
    render_coverage(&mut out, model);
    render_rollout(&mut out, model);
    render_mismatches(&mut out, model);
    render_counters(&mut out, model);
    render_profile(&mut out, model);
    render_footer(&mut out);

    out.push_str("</main>\n</body>\n</html>\n");
    out
}

fn render_banner(out: &mut String, model: &PageModel) {
    let banner = &model.banner;
    let _ = writeln!(
        out,
        "<section class=\"banner {}\">\n<div class=\"state\">{}</div>",
        banner.state.class(),
        esc(banner.state.headline())
    );
    let _ = writeln!(out, "<p class=\"note\">{}</p>", esc(banner.state.note()));
    bullets(out, "Failures", &banner.failures);
    bullets(out, "Missing required inputs", &banner.incomplete);
    bullets(out, "Optional inputs not provided", &banner.notes);
    out.push_str("</section>\n");
}

/// One row of the inputs manifest.
struct ManifestRow<'a> {
    name: &'a str,
    path: Option<&'a Path>,
    status: &'a str,
    class: &'a str,
    /// Whether a green banner needs this input.
    required: bool,
    detail: String,
}

impl<'a> ManifestRow<'a> {
    /// A row whose whole story is its section's standing: parsed, absent, or
    /// unavailable-for-this-reason. Only the verdict row says more.
    fn of<T>(
        name: &'a str,
        path: Option<&'a Path>,
        section: &Section<T>,
        required: bool,
    ) -> ManifestRow<'a> {
        ManifestRow {
            name,
            path,
            status: section.word(),
            class: section.class(),
            required,
            detail: match section {
                Section::Unavailable(why) => why.clone(),
                _ => String::new(),
            },
        }
    }

    fn write(&self, out: &mut String) {
        let path = self
            .path
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "<tr><td>{}</td><td>{}</td><td class=\"mono\" title=\"{}\">{}</td><td>{}</td>\
             <td>{}</td><td>{}</td></tr>",
            esc(self.name),
            present(self.path.is_some()),
            esc(&path),
            esc(&path),
            pill(self.class, self.status),
            if self.required {
                "required for a clean result"
            } else {
                "optional"
            },
            esc(&self.detail),
        );
    }
}

fn render_manifest(out: &mut String, model: &PageModel) {
    let e = &model.evidence;
    out.push_str("<h2>1. Inputs</h2>\n<table>\n<tr><th>Input</th><th>Provided</th>");
    out.push_str("<th>Path</th><th>Status</th><th>Role</th><th>Detail</th></tr>\n");

    let i = &model.inputs;
    // The sink directory is always provided — the CLI requires `--dir`.
    ManifestRow::of("sink directory", Some(i.sink_dir.as_path()), &e.sink, true).write(out);
    ManifestRow {
        // The one row that reports more than its own readability: which verdict
        // was read, so the page names the outcome it was handed.
        detail: match &e.verdict {
            Section::Ok(VerdictArtifact::InputUnavailable(dto)) => {
                format!("input-unavailable (exit {}): {}", dto.exit_code, dto.error)
            }
            Section::Ok(VerdictArtifact::Full(v)) => {
                format!("{} (exit {}), mode {}", v.verdict, v.exit_code, v.mode)
            }
            Section::Unavailable(why) => why.clone(),
            Section::NotProvided => String::new(),
        },
        ..ManifestRow::of("verdict artifact", i.verdict.as_deref(), &e.verdict, true)
    }
    .write(out);
    ManifestRow::of("config", i.config.as_deref(), &e.config, true).write(out);
    ManifestRow::of("observe profile", i.profile.as_deref(), &e.profile, false).write(out);
    ManifestRow::of("metrics scrape", i.metrics.as_deref(), &e.metrics, false).write(out);
    out.push_str("</table>\n");
}

/// The five gating checks, as the verdict recorded them.
///
/// The verdict is where a campaign is actually decided, so the page shows the
/// checks rather than only the exit code they add up to: an operator reading a
/// red banner needs to see *which* check failed, and one reading an offline
/// verdict needs to see the four that were never taken at all. Each row's color
/// comes from that check's own status, never from the document's exit code —
/// a `fail` beside an `exit_code: 0` renders red here and is separately named
/// as an inconsistency by [`semantic_violations`].
fn render_checks(out: &mut String, model: &PageModel) {
    out.push_str("<h2>2. Verdict checks</h2>\n");
    let verdict = match &model.evidence.verdict {
        // Warn, not neutral: the subject of this section is the checks, and
        // when there is no verdict none of them was taken.
        Section::NotProvided => {
            return note(
                out,
                "warn",
                "NOT PROVIDED",
                "No verdict artifact was provided, so none of the five gating checks was \
                 taken against this campaign.",
            )
        }
        Section::Unavailable(why) => return note(out, "bad", "UNAVAILABLE", why),
        Section::Ok(VerdictArtifact::InputUnavailable(dto)) => {
            return note(
                out,
                "bad",
                "NO CHECKS TAKEN",
                &format!(
                    "The verdict run ended before it could check anything (exit {}): {}",
                    dto.exit_code, dto.error
                ),
            )
        }
        Section::Ok(VerdictArtifact::Full(v)) => v,
    };

    out.push_str("<table>\n<tr><th>Check</th><th>Status</th><th>Detail</th></tr>\n");
    for (name, check) in verdict.checks.named() {
        let mut detail = esc(&check.detail);
        // The canary count belongs to the check that would have used it, and
        // nowhere else on the page has a home for it.
        if name == "canary" {
            let _ = write!(
                detail,
                "{}{} canary record(s) counted, excluded from the mismatch total",
                if detail.is_empty() { "" } else { " — " },
                verdict.canary_records
            );
        }
        let _ = writeln!(
            out,
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(name),
            check.status_pill(),
            detail
        );
    }
    out.push_str("</table>\n");
}

fn render_routes(out: &mut String, model: &PageModel) {
    out.push_str("<h2>3. Configured routes</h2>\n");
    match &model.evidence.config {
        Section::NotProvided => {
            out.push_str(
                "<p class=\"note\">No config was provided, so the route table this \
                          campaign ran under is unknown. Route ids below come from artifacts \
                          alone and cannot be checked against anything.</p>\n",
            );
        }
        Section::Unavailable(why) => note(out, "bad", "UNAVAILABLE", why),
        Section::Ok(view) if view.routes.is_empty() => {
            out.push_str("<p class=\"note\">The config declares no routes.</p>\n");
        }
        Section::Ok(view) => {
            out.push_str(
                "<table>\n<tr><th>Route</th><th>Mode</th><th>Comparison</th>\
                 <th class=\"num\">Sample rate</th><th class=\"num\">Floor</th>\
                 <th>Floors row expected</th></tr>\n",
            );
            for route in &view.routes {
                let _ = writeln!(
                    out,
                    "<tr>{}<td>{}</td><td>{}</td><td class=\"num\">{:.2}</td>\
                     <td class=\"num\">{}</td><td>{}</td></tr>",
                    route_cell(&route.id),
                    esc(route.mode.as_str()),
                    if route.comparison_enabled {
                        pill("good", "enabled")
                    } else {
                        pill("neutral", "disabled")
                    },
                    route.sample_rate,
                    // Only a route the verdict is expected to floor has a
                    // floor worth printing; the default 1 on a disabled route
                    // asserts nothing.
                    num(route.expects_floor_row.then_some(route.floor)),
                    present(route.expects_floor_row),
                );
            }
            out.push_str("</table>\n");
        }
    }
}

fn render_coverage(out: &mut String, model: &PageModel) {
    out.push_str("<h2>4. Coverage against floors</h2>\n");
    bullets(out, "Drift findings", &model.evidence.drift);
    if model.routes.is_empty() {
        out.push_str("<p class=\"note\">No route id appears in any input.</p>\n");
        return;
    }
    out.push_str(
        "<table>\n<tr><th>Route</th><th>Floors</th><th class=\"num\">Comparisons</th>\
         <th class=\"num\">Floor</th><th>Config</th><th>Verdict</th><th>Sink</th>\
         <th>Metrics</th><th>Profile</th></tr>\n",
    );
    for row in &model.routes {
        let _ = writeln!(
            out,
            "<tr>{}<td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td>{}</td>\
             <td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            route_cell(&row.id),
            pill(row.floor_class.class(), row.floor_class.word()),
            num(row.comparisons),
            num(row.floor),
            present(row.in_config),
            present(row.in_verdict),
            present(row.in_sink),
            present(row.in_metrics),
            present(row.in_profile),
        );
    }
    out.push_str("</table>\n");
}

/// A percentage, printed the way the scrape carried it: no invented precision,
/// no trailing `.0` on a whole number.
fn pct(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}%")
    } else {
        format!("{value}%")
    }
}

/// A cell whose value the scrape did not settle: the pill says so and the
/// sentence says why. Never an em dash on its own here — a blank cell in a
/// rollout table reads as nothing to report.
fn unsettled(word: &str, why: &str) -> String {
    format!("<td>{} {}</td>", pill("warn", word), esc(why))
}

/// A cell the config rules out. The reason rides the `title` because the
/// column is about routes for which it *does* apply.
fn inapplicable(why: &str) -> String {
    format!("<td title=\"{}\">—</td>", esc(why))
}

/// The two arms every rollout cell answers the same way; only the known arm is
/// the column's own business. [`observed_cell`] is the deliberate exception —
/// an unread share is a traffic statement, not an UNAVAILABLE one.
fn reading_cell<T>(reading: &Reading<T>, known: impl FnOnce(&T) -> String) -> String {
    match reading {
        Reading::Known(value) => known(value),
        Reading::NotApplicable(why) => inapplicable(why),
        Reading::Unknown(why) => unsettled("UNAVAILABLE", why),
    }
}

/// The resolved-target cell. When the provider is stale the number never
/// travels alone: a bare `0%` reads as a rollout somebody turned down, and the
/// whole point of this section is that it was displaced instead.
fn target_cell(target: &Reading<f64>, flags: &FlagReading) -> String {
    reading_cell(target, |value| {
        if flags.known().is_some_and(|f| f.stale) {
            format!("<td>{} — fail-safe (flags stale)</td>", pct(*value))
        } else {
            format!("<td>{}</td>", pct(*value))
        }
    })
}

/// The observed-share cell: the percentage *and* the counts behind it, because
/// "90%" over ten requests and over ten thousand are different facts.
fn observed_cell(observed: &Reading<ObservedShare>) -> String {
    let count = |side: Option<u64>| match side {
        Some(count) => count.to_string(),
        // Never a bare `0`: an absent series and a counted zero are different
        // facts, and this column is where a rollout at 0% would otherwise be
        // indistinguishable from one whose counters were lost.
        None => "no series = zero recorded".to_string(),
    };
    match observed {
        Reading::Known(share) => {
            let detail = format!(
                "(new: {} / legacy: {})",
                count(share.new),
                count(share.legacy)
            );
            match share.percentage() {
                // The one rounded number on the page, to a tenth — and the only
                // one that may be, because it is *derived* rather than read,
                // and the counts it came from sit beside it unrounded.
                Some(percentage) => format!(
                    "<td>{} {}</td>",
                    pct((percentage * 10.0).round() / 10.0),
                    esc(&detail)
                ),
                // Both sides zero — whether counted or absent, no request on
                // this route was recorded, and there is no share to state.
                None => format!(
                    "<td>{} no traffic recorded {}</td>",
                    pill("warn", "NO SHARE"),
                    esc(&detail)
                ),
            }
        }
        Reading::NotApplicable(why) => inapplicable(why),
        Reading::Unknown(why) => format!("<td>{} {}</td>", pill("warn", "NO SHARE"), esc(why)),
    }
}

/// The breaker cell: the state's name and its own color, never the gauge's
/// number. "2" on a status page is not a state anybody reads.
///
/// A one-step skew between the gauge and the transition history says so in the
/// cell. The race is benign — the scrape handler refreshes the gauge and then
/// renders — but a page that silently picked one of the two readings would be
/// presenting a coin flip as a measurement.
fn breaker_cell(truth: &RouteTruth) -> String {
    let cell = reading_cell(&truth.breaker_state, |state| {
        let (class, word) = match state {
            BreakerState::Closed => ("good", "closed"),
            BreakerState::HalfOpen => ("warn", "half-open"),
            BreakerState::Open => ("bad", "open"),
        };
        format!("<td>{}</td>", pill(class, word))
    });
    let Some(implied) = truth.state_skew else {
        return cell;
    };
    // Reopen the cell to append the caveat: the reported state is the more
    // diverting of the two, and the counters' reading is named beside it.
    let inner = cell
        .strip_prefix("<td>")
        .and_then(|c| c.strip_suffix("</td>"))
        .unwrap_or(&cell)
        .to_string();
    format!(
        "<td>{inner} {} the transition counts describe a breaker that is {} — a transition landed \
         between the gauge refresh and this render, so the more diverting of the two is \
         reported</td>",
        pill("warn", "STATE/COUNTERS SKEWED"),
        esc(implied.as_str())
    )
}

/// The four transition counts, compact: `closed→open 2, open→half-open 1, …`.
/// All four always, including the zeros — a breaker that never opened is the
/// answer a rollout review is looking for, and omitting the zero would leave
/// the reader to guess whether it was zero or unknown.
fn transitions_cell(transitions: &Reading<[u64; 4]>) -> String {
    reading_cell(transitions, |counts| {
        let display = |state: BreakerState| match state {
            BreakerState::HalfOpen => "half-open",
            other => other.as_str(),
        };
        let text = BreakerState::TRANSITIONS
            .iter()
            .zip(counts.iter())
            .map(|((from, to), count)| format!("{}→{} {count}", display(*from), display(*to)))
            .collect::<Vec<_>>()
            .join(", ");
        format!("<td>{}</td>", esc(&text))
    })
}

/// The rollout's own section: what each route targeted, what it served, and
/// what was answering for it.
///
/// The three-way semantics are the same ones the rest of the page runs on, with
/// one addition specific to this section: a config with no rollout route at all
/// is an *honest empty* — one line, no table, no finding — because a page that
/// rendered a red "unavailable" over a campaign that never had a rollout would
/// be crying wolf on every shadow-only deployment.
fn render_rollout(out: &mut String, model: &PageModel) {
    out.push_str("<h2>5. Rollout &amp; resilience</h2>\n");
    match &model.evidence.rollout {
        // Mirrors `render_counters`: no scrape is an absence of evidence. The
        // config's rollout settings are deliberately *not* shown on their own
        // here — a table of what a rollout was asked to do, on a page about
        // what a campaign did, reads as a report that something was checked.
        Section::NotProvided => note(
            out,
            "neutral",
            "NOT PROVIDED",
            "No metrics scrape was captured, so nothing is known about what any rollout targeted, \
             what it served, or whether a circuit breaker or a stale flag provider was answering \
             for it. This is an absence of evidence, not a rollout at 0%.",
        ),
        Section::Unavailable(why) => note(out, "bad", "UNAVAILABLE", why),
        Section::Ok(view) => render_rollout_view(out, view),
    }
}

fn render_rollout_view(out: &mut String, view: &RolloutResilienceView) {
    match view.scope {
        RolloutScope::NoRolloutRoutes => {
            return note(
                out,
                "neutral",
                "NO ROLLOUT",
                "This config declares no percentage_split or failover_to_legacy route, so there \
                 is no rollout to report on.",
            )
        }
        RolloutScope::Unchecked => note(
            out,
            "warn",
            "UNCHECKED",
            "No config was provided, so which routes this scrape *should* carry rollout series \
             for is unknown and no per-route reading below could be required of it. Only the \
             process-wide flag-provider gauges are readable.",
        ),
        RolloutScope::Configured => {}
    }

    // The provider block comes first: it is the single fact that can displace
    // every row below, and a reader who meets the rows first will read a table
    // of zeros before learning why they are zeros.
    render_flag_provider(out, view);

    if view.rows.is_empty() {
        return;
    }
    render_rollout_rows(out, view);
    render_rollout_config(out, &view.rows);
}

/// The flag provider's own standing, as a note above the table.
fn render_flag_provider(out: &mut String, view: &RolloutResilienceView) {
    match &view.flags {
        FlagReading::NotApplicable => {}
        FlagReading::Absent(why) => note(out, "bad", "FLAG PROVIDER UNKNOWN", why),
        // Named apart from an absence: these three gauges come from one
        // `health()` snapshot, so a tuple that disagrees with itself is a
        // scrape that was edited, merged, or is not limen's.
        FlagReading::Contradiction(why) => note(out, "bad", "FLAG PROVIDER INCOHERENT", why),
        FlagReading::Known(flags) => {
            let age = match flags.staleness_seconds {
                Some(seconds) => format!("Last successful refresh {seconds}s ago"),
                None => "No successful refresh has ever been recorded".to_string(),
            };
            let failures = format!(
                "{} consecutive failed refresh(es) since the last success.",
                flags.consecutive_failures
            );
            if flags.stale {
                note(
                    out,
                    "bad",
                    "STALE",
                    &format!(
                        "The flag provider is stale, so every percentage_split route is displaced \
                         by fail-safe {}: their resolved target is 0% whatever the flag says, and \
                         a 0% here is a rollout that was switched off for them rather than one \
                         that was turned down. {age}. {failures}",
                        view.fail_safe_mode.map_or("mode", fail_safe_name)
                    ),
                );
            } else {
                note(out, "good", "FRESH", &format!("{age}. {failures}"));
            }
        }
    }
}

/// What each route actually did, one row per rollout route.
fn render_rollout_rows(out: &mut String, view: &RolloutResilienceView) {
    out.push_str(
        "<table>\n<tr><th>Route</th><th>Mode</th><th>Resolved target</th>\
         <th>Observed new share</th><th>Breaker</th><th>Breaker transitions</th>\
         <th>Failover-safe</th></tr>\n",
    );
    for row in &view.rows {
        if !row.rejected.is_empty() {
            // One spanning cell rather than five unsettled ones: the row was
            // rejected as a whole, and five separate "unavailable" cells would
            // suggest five separate readings were attempted and lost.
            let _ = writeln!(
                out,
                "<tr>{}<td>{}</td><td colspan=\"5\">{} {}</td></tr>",
                route_cell(&row.id),
                esc(row.mode.as_str()),
                pill("bad", "UNAVAILABLE"),
                esc(&row.rejected.join(" "))
            );
            continue;
        }
        let _ = writeln!(
            out,
            "<tr>{}<td>{}</td>{}{}{}{}<td>{}</td></tr>",
            route_cell(&row.id),
            esc(row.mode.as_str()),
            target_cell(&row.truth.target, &view.flags),
            observed_cell(&row.truth.observed),
            breaker_cell(&row.truth),
            transitions_cell(&row.truth.transitions),
            present(row.failover_safe),
        );
    }
    out.push_str("</table>\n");
}

/// The breaker's tuning as a sentence: the numbers that decide what an open
/// breaker on this route even means, and whether anything ever asks it.
fn breaker_prose(breaker: &BreakerSettings, consulted: bool) -> String {
    if !breaker.enabled {
        return "disabled".to_string();
    }
    format!(
        "enabled — opens above a failure rate of {} over at least {} request(s), stays open \
         {}ms, then admits {} trial request(s){}",
        breaker.failure_rate_threshold,
        breaker.min_requests,
        breaker.open_duration_ms,
        breaker.half_open_max_requests,
        if consulted {
            ""
        } else {
            " (never consulted in this mode)"
        }
    )
}

/// What the config asked for, beside what happened — a separate table from the
/// one above so no configured number can be mistaken for a reading.
fn render_rollout_config(out: &mut String, rows: &[RolloutRow]) {
    out.push_str("<h3>As configured</h3>\n");
    out.push_str(
        "<table>\n<tr><th>Route</th><th>Mode</th><th>Rollout flag</th>\
         <th class=\"num\">Default</th><th>Assignment key</th><th>Circuit breaker</th>\
         <th>Failover-safe</th></tr>\n",
    );
    for row in rows {
        let (flag, default, key) = match &row.rollout {
            Some(rollout) => (
                rollout.percentage_flag.clone(),
                format!("{}", rollout.default_percentage),
                match &rollout.assignment_header {
                    Some(header) => format!(
                        "header {header}, falling back to {}",
                        rollout.assignment_fallback
                    ),
                    None => rollout.assignment_fallback.clone(),
                },
            ),
            None => ("—".to_string(), "—".to_string(), "—".to_string()),
        };
        let breaker = breaker_prose(&row.breaker, row.breaker_consulted);
        let _ = writeln!(
            out,
            "<tr>{}<td>{}</td><td class=\"mono\">{}</td><td class=\"num\">{}</td><td>{}</td>\
             <td>{}</td><td>{}</td></tr>",
            route_cell(&row.id),
            esc(row.mode.as_str()),
            esc(&flag),
            esc(&default),
            esc(&key),
            esc(&breaker),
            present(row.failover_safe),
        );
    }
    out.push_str("</table>\n");
}

fn render_mismatches(out: &mut String, model: &PageModel) {
    out.push_str("<h2>6. Mismatches</h2>\n");
    let state = &model.evidence.sink_state;
    note(out, state.class(), state.word(), state.prose());

    let Some(report) = model.evidence.sink.get() else {
        return;
    };
    let sink_counts = &model.evidence.sink_counts;
    let _ = writeln!(
        out,
        "<p class=\"note\">{} file(s) read, {} record(s) — {} mismatch(es), {} unparseable \
         line(s).</p>",
        report.files_read, report.total, sink_counts.mismatches, report.malformed_lines
    );
    // The canary is limen's own record, written to prove the pipeline records
    // at all. `limen verdict` excludes it from the mismatch total and reports
    // it separately; counting it here would make the evidence of a healthy
    // sink into a reason to call the run dirty.
    if sink_counts.canary > 0 {
        let vouched = model
            .evidence
            .full_verdict()
            .map(|v| {
                if v.canary_records == sink_counts.canary as u64 {
                    format!(" The verdict counted the same {}.", v.canary_records)
                } else {
                    format!(" The verdict counted {} — they disagree.", v.canary_records)
                }
            })
            .unwrap_or_default();
        note(
            out,
            "good",
            "CANARY RECORDS",
            &format!(
                "{} record(s) under {CANARY_ROUTE_ID}, excluded from the mismatch count above. \
                 They are not findings: they are limen proving the record → flush → report \
                 pipeline bites.{vouched}",
                sink_counts.canary
            ),
        );
    }
    for (id, count) in &sink_counts.other_reserved {
        note(
            out,
            "bad",
            "UNKNOWN RESERVED ID",
            &format!(
                "{count} record(s) under {id}. The {RESERVED_ROUTE_ID_PREFIX} namespace is \
                 limen's own and nothing limen writes uses that id."
            ),
        );
    }
    if report.malformed_lines > 0 {
        let _ = writeln!(
            out,
            "<p class=\"note\">{} Unparseable lines mean records are missing from every count \
             on this page.</p>",
            pill("bad", "TORN RECORDS")
        );
    }
    // Reserved ids are accounted for above; the per-route tables are the
    // mismatch answer, and `verdict::sink_mismatches_by_route` excludes them
    // from its copy of it for the same reason.
    let mismatch_routes: Vec<&sink::RouteReport> = report
        .routes
        .iter()
        .filter(|r| !is_reserved(&r.route_id))
        .collect();
    if mismatch_routes.is_empty() {
        return;
    }
    out.push_str("<table>\n<tr><th>Route</th><th class=\"num\">Count</th><th>Kinds</th></tr>\n");
    for route in &mismatch_routes {
        let _ = writeln!(
            out,
            "<tr>{}<td class=\"num\">{}</td><td>{}</td></tr>",
            route_cell(&route.route_id),
            route.count,
            esc(&counts(&route.kinds))
        );
    }
    out.push_str("</table>\n");

    for route in &mismatch_routes {
        if route.examples.is_empty() {
            continue;
        }
        let _ = writeln!(
            out,
            "<h3>{} — {} most recent</h3>",
            esc(&route.route_id),
            route.examples.len().min(EXAMPLES_SHOWN)
        );
        out.push_str(
            "<table>\n<tr><th>Timestamp</th><th>Method</th><th>Path</th><th>Request id</th>\
             <th>Kinds</th></tr>\n",
        );
        for example in route.examples.iter().take(EXAMPLES_SHOWN) {
            let _ = writeln!(
                out,
                "<tr><td class=\"mono\">{}</td><td>{}</td><td class=\"mono\">{}</td>\
                 <td class=\"mono\">{}</td><td>{}</td></tr>",
                esc(&example.timestamp),
                esc(&example.method),
                esc(&example.path),
                esc(&example.request_id),
                esc(&example.mismatch_kinds.join(", "))
            );
        }
        out.push_str("</table>\n");
    }
}

fn render_counters(out: &mut String, model: &PageModel) {
    out.push_str("<h2>7. Runtime counters</h2>\n");
    match &model.evidence.metrics {
        Section::NotProvided => note(
            out,
            "neutral",
            "NOT PROVIDED",
            "No metrics scrape was captured, so no runtime counter is known. This is an \
             absence of evidence, not a set of zeros.",
        ),
        Section::Unavailable(why) => note(out, "bad", "UNAVAILABLE", why),
        Section::Ok(view) => {
            for family in &view.families {
                let _ = writeln!(out, "<h3>{}</h3>", esc(&family.name));
                if !family.present {
                    // Stated, not skipped: an absent family is a fact about the
                    // scrape, and the note says which of limen's own tools
                    // tolerates it and why.
                    note(out, "warn", "ABSENT", family.absence.note());
                    continue;
                }
                if family.rows.is_empty() {
                    note(
                        out,
                        "warn",
                        "NO SAMPLES",
                        "The family is exported but carries no samples.",
                    );
                    continue;
                }
                out.push_str(
                    "<table>\n<tr><th>Route</th><th>Labels</th><th class=\"num\">Count</th>\
                     </tr>\n",
                );
                for row in &family.rows {
                    let _ = writeln!(
                        out,
                        "<tr>{}<td>{}</td><td class=\"num\">{}</td></tr>",
                        route_label_cell(row.route.as_ref()),
                        esc(&row.labels),
                        row.value
                    );
                }
                out.push_str("</table>\n");
            }
        }
    }

    // The verdict's own non-gating counters, when it carried any.
    if let Some(v) = model.evidence.full_verdict() {
        if !v.informational.is_empty() {
            out.push_str("<h3>Skip and failure counters recorded by the verdict</h3>\n");
            out.push_str(
                "<table>\n<tr><th>Metric</th><th>Route</th><th>Reason</th>\
                 <th class=\"num\">Count</th></tr>\n",
            );
            for info in &v.informational {
                let _ = writeln!(
                    out,
                    "<tr><td class=\"mono\">{}</td><td class=\"mono\">{}</td><td>{}</td>\
                     <td class=\"num\">{}</td></tr>",
                    esc(&info.metric),
                    esc(&info.route),
                    esc(&info.reason),
                    info.value
                );
            }
            out.push_str("</table>\n");
        }
    }
}

fn render_profile(out: &mut String, model: &PageModel) {
    out.push_str("<h2>8. Observe profile</h2>\n");
    match &model.evidence.profile {
        Section::NotProvided => note(
            out,
            "neutral",
            "NOT PROVIDED",
            "No observe profile was captured.",
        ),
        Section::Unavailable(why) => note(out, "bad", "UNAVAILABLE", why),
        Section::Ok(profile) if profile.routes.is_empty() => {
            let _ = writeln!(
                out,
                "<p class=\"note\">Recorded at sample rate {:.4}. The profile carries no \
                 routes.</p>",
                profile.sample_rate
            );
        }
        Section::Ok(profile) => {
            let _ = writeln!(
                out,
                "<p class=\"note\">Recorded at sample rate {:.4}.{}</p>",
                profile.sample_rate,
                if profile.sample_rate < 1.0 {
                    " A sampled profile describes a fraction of the traffic, not all of it."
                } else {
                    ""
                }
            );
            out.push_str(
                "<table>\n<tr><th>Route</th><th class=\"num\">Observations</th>\
                 <th class=\"num\">Reads</th><th class=\"num\">Writes</th>\
                 <th class=\"num\">Transport errors</th><th>Methods</th>\
                 <th>Status classes</th><th class=\"num\">No length</th>\
                 <th class=\"num\">Set-Cookie</th><th class=\"num\">Redirects</th>\
                 <th>Overflow flags</th></tr>\n",
            );
            for (id, route) in &profile.routes {
                let mut flags = Vec::new();
                for (set, name) in [
                    (route.query_names_overflow, "query names"),
                    (route.distinct_read_paths_overflow, "read paths"),
                    (route.content_types_overflow, "content types"),
                    (route.fingerprint_overflow, "fingerprints"),
                ] {
                    if set {
                        flags.push(pill("warn", name));
                    }
                }
                let flags = if flags.is_empty() {
                    pill("neutral", "none")
                } else {
                    flags.join(" ")
                };
                let _ = writeln!(
                    out,
                    "<tr>{}<td class=\"num\">{}</td><td class=\"num\">{}</td>\
                     <td class=\"num\">{}</td><td class=\"num\">{}</td><td>{}</td><td>{}</td>\
                     <td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td>\
                     <td>{}</td></tr>",
                    route_cell(id),
                    route.observations,
                    route.reads,
                    route.writes,
                    route.transport_errors,
                    esc(&counts(&route.methods)),
                    esc(&counts(&route.status_classes)),
                    route.length_missing,
                    route.set_cookie_reads,
                    route.redirect_reads,
                    flags,
                );
            }
            out.push_str("</table>\n");
        }
    }
}

fn render_footer(out: &mut String) {
    out.push_str(
        "<footer>\n<p>Rendered by <code>limen report --format html</code> from the artifacts \
         named in the inputs table. This page runs nothing and contacts nothing: it reports \
         only what those files already said.</p>\n<p>Beyond the drift checks listed above, \
         artifact provenance is not verified — nothing here proves the files describe the same \
         run, the same deployment, or the same build.</p>\n</footer>\n",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dto(json: serde_json::Value) -> VerdictDto {
        serde_json::from_value(json).expect("verdict dto")
    }

    /// A coherent clean verdict.
    fn clean_verdict() -> serde_json::Value {
        serde_json::json!({
            "mode": "online",
            "verdict": "clean",
            "exit_code": 0,
            "checks": {
                "drain": {"status": "pass", "detail": "quiesced"},
                "floors": {"status": "pass", "detail": "1 floored route"},
                "sink_integrity": {"status": "pass", "detail": "agree"},
                "canary": {"status": "skipped", "detail": "not requested"},
                "mismatches": {"status": "pass", "detail": "zero"},
            },
            "mismatches_total": 0,
            "canary_records": 0,
            "floors": [{"route_id": "a", "comparisons": 3, "floor": 1, "met": true}],
            "sink_mismatches_by_route": {},
            "informational": [],
        })
    }

    #[test]
    fn esc_neutralizes_text_and_attribute_payloads() {
        let hostile = "<script>alert(\"x\" & 'y')</script>";
        let escaped = esc(hostile);
        assert!(!escaped.contains('<'), "{escaped}");
        assert!(!escaped.contains('>'), "{escaped}");
        assert!(!escaped.contains('"'), "{escaped}");
        assert!(!escaped.contains('\''), "{escaped}");
        assert!(escaped.contains("&lt;script&gt;"), "{escaped}");
        assert!(escaped.contains("&quot;"), "{escaped}");
        assert!(escaped.contains("&#39;"), "{escaped}");
        // The ampersand is escaped first, so nothing double-decodes.
        assert!(escaped.contains("&amp;"), "{escaped}");
    }

    #[test]
    fn a_coherent_clean_verdict_has_no_violations() {
        assert!(semantic_violations(&dto(clean_verdict())).is_empty());
    }

    #[test]
    fn exit_zero_must_match_the_verdict_name_and_the_checks() {
        let mut v = clean_verdict();
        v["exit_code"] = serde_json::json!(10);
        assert!(semantic_violations(&dto(v))
            .iter()
            .any(|s| s.contains("expected 0")));

        let mut v = clean_verdict();
        v["checks"]["sink_integrity"]["status"] = serde_json::json!("fail");
        assert!(semantic_violations(&dto(v))
            .iter()
            .any(|s| s.contains("failed sink integrity check")));

        let mut v = clean_verdict();
        v["mismatches_total"] = serde_json::json!(4);
        assert!(semantic_violations(&dto(v))
            .iter()
            .any(|s| s.contains("4 mismatch(es) counted")));
    }

    #[test]
    fn a_met_flag_must_follow_from_the_counts() {
        let mut v = clean_verdict();
        v["floors"][0]["comparisons"] = serde_json::json!(0);
        // met:true with 0 comparisons against a floor of 1 is a contradiction.
        let violations = semantic_violations(&dto(v));
        assert!(
            violations.iter().any(|s| s.contains("claims met=true")),
            "{violations:?}"
        );
    }

    #[test]
    fn the_floors_check_must_agree_with_its_own_rows() {
        let mut v = clean_verdict();
        v["exit_code"] = serde_json::json!(20);
        v["verdict"] = serde_json::json!("floors-unmet");
        v["floors"][0]["met"] = serde_json::json!(false);
        v["floors"][0]["comparisons"] = serde_json::json!(0);
        // The rows say unmet but the check still reports pass.
        let violations = semantic_violations(&dto(v));
        assert!(
            violations
                .iter()
                .any(|s| s.contains("floors check reports")),
            "{violations:?}"
        );
    }

    #[test]
    fn an_unrecognized_verdict_name_may_not_exit_zero() {
        let mut v = clean_verdict();
        v["verdict"] = serde_json::json!("probably-fine");
        let violations = semantic_violations(&dto(v));
        assert!(
            violations.iter().any(|s| s.contains("unrecognized")),
            "{violations:?}"
        );
    }

    #[test]
    fn a_verdict_dto_tolerates_unknown_fields_but_not_missing_core_ones() {
        let mut v = clean_verdict();
        v["a_field_from_a_newer_limen"] = serde_json::json!({"nested": true});
        assert!(serde_json::from_value::<VerdictDto>(v).is_ok());

        // An arbitrary object must not read as an empty-but-valid verdict.
        assert!(serde_json::from_value::<VerdictDto>(serde_json::json!({})).is_err());
        assert!(serde_json::from_value::<VerdictDto>(serde_json::json!({"hello": 1})).is_err());
    }

    /// The four series `register_verdict_series` pre-touches at startup — the
    /// exposition any live limen renders from its very first scrape.
    const REGISTERED: &str = "\
limen_shadow_in_flight 0
limen_diff_sink_enqueued_total 0
limen_diff_sink_written_total 0
limen_diff_sink_dropped_total{reason=\"queue_full\"} 0
limen_diff_sink_dropped_total{reason=\"io_error\"} 0
limen_diff_sink_dropped_total{reason=\"writer_gone\"} 0
";

    /// The page must require exactly what `limen verdict` requires — no more,
    /// or a page renders FAILURE against a run the gate itself passed.
    #[test]
    fn the_required_families_are_verdicts_required_series() {
        let ours: BTreeSet<&str> = FAMILIES
            .iter()
            .filter(|(_, a)| *a == Absence::Required)
            .map(|(name, _)| *name)
            .collect();
        let theirs: BTreeSet<&str> = crate::verdict::REQUIRED_SERIES.into_iter().collect();
        assert_eq!(ours, theirs);
    }

    #[test]
    fn a_scrape_missing_a_required_family_is_unavailable_not_zero() {
        // Everything a busy proxy exports — except one series limen registers
        // at startup, whose absence means this is not a limen control plane.
        let scrape = Scrape::parse(
            "limen_shadow_in_flight 0\n\
             limen_diff_sink_enqueued_total 0\n\
             limen_diff_sink_written_total 0\n\
             limen_comparisons_total{route=\"a\",result=\"match\"} 1\n",
        )
        .unwrap();
        let err = MetricsView::from_scrape(&scrape).unwrap_err();
        assert!(err.contains(DIFF_SINK_DROPPED_TOTAL), "{err}");
        assert!(err.contains("never a zero count"), "{err}");
    }

    /// The bug a real run found: a service that never skipped a comparison
    /// exports no `limen_comparison_skipped_total` at all, because that
    /// counter is registered on its first event. The live verdict against that
    /// same process exits 0, so the page must render it too.
    #[test]
    fn a_lazily_registered_family_may_be_absent() {
        let scrape = Scrape::parse(&format!(
            "{REGISTERED}limen_comparisons_total{{route=\"a\",result=\"match\"}} 3\n\
             limen_shadow_requests_total{{route=\"a\"}} 3\n"
        ))
        .unwrap();
        let view = MetricsView::from_scrape(&scrape).expect("a quiet proxy is still a proxy");
        let absent: Vec<&str> = view
            .families
            .iter()
            .filter(|f| !f.present)
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(
            absent,
            [
                COMPARISON_SKIPPED_TOTAL,
                SHADOW_SKIPPED_TOTAL,
                SHADOW_FAILED_TOTAL
            ],
            "only the lazily-registered families are absent"
        );
        // …and every one of them says so on the page rather than reading as a
        // silent zero.
        for family in view.families.iter().filter(|f| !f.present) {
            assert_eq!(family.absence, Absence::Informational);
            assert!(family.absence.note().contains("first event of its kind"));
        }
    }

    /// `limen_comparisons_total` is the one family verdict reads as zero when
    /// absent, and it says why rather than rendering nothing.
    #[test]
    fn an_absent_comparisons_family_is_noted_as_verdicts_zero() {
        let scrape = Scrape::parse(REGISTERED).unwrap();
        let view = MetricsView::from_scrape(&scrape).expect("a proxy that served nothing");
        let comparisons = view
            .families
            .iter()
            .find(|f| f.name == COMPARISONS_TOTAL)
            .unwrap();
        assert!(!comparisons.present);
        assert_eq!(comparisons.absence, Absence::ReadsAsZero);
        assert!(comparisons.absence.note().contains("reads this as zero"));
    }

    #[test]
    fn an_empty_scrape_is_unavailable() {
        let scrape = Scrape::parse("# HELP nothing\n# TYPE nothing counter\n").unwrap();
        assert!(MetricsView::from_scrape(&scrape).is_err());
        assert!(MetricsView::from_scrape(&Scrape::default()).is_err());
    }

    #[test]
    fn fractional_and_negative_counts_are_refused() {
        let text = |value: &str| {
            format!(
                "{REGISTERED}limen_comparisons_total{{route=\"a\",result=\"match\"}} {value}\n\
                 limen_comparison_skipped_total{{route=\"a\",reason=\"event_stream\"}} 1\n\
                 limen_shadow_requests_total{{route=\"a\"}} 1\n\
                 limen_shadow_failed_total{{route=\"a\",reason=\"timeout\"}} 0\n"
            )
        };
        for bad in [
            "1.5",
            "-3",
            // 2^64: finite, integral and non-negative as an `f64`, and
            // `as u64` would saturate it to `u64::MAX` — a count no scrape
            // ever carried, rendered on a page that would otherwise be green.
            "18446744073709551616",
            "1e3",
            "NaN",
        ] {
            let scrape = Scrape::parse(&text(bad)).unwrap();
            let err = MetricsView::from_scrape(&scrape).unwrap_err();
            assert!(err.contains("exact non-negative integer"), "{bad}: {err}");
            assert!(err.contains(bad), "{bad}: {err} does not quote the value");
        }
        // Counts an `f64` cannot hold are rendered exactly, not rounded: the
        // raw token is the source of truth. 2^53 + 1 is the first casualty of
        // the float path, and `u64::MAX` its last.
        for exact in [9_007_199_254_740_993u64, u64::MAX] {
            let scrape = Scrape::parse(&text(&exact.to_string())).unwrap();
            let view = MetricsView::from_scrape(&scrape).unwrap();
            assert_eq!(view.families[0].rows[0].value, exact);
        }
        let scrape = Scrape::parse(&text("2")).unwrap();
        let view = MetricsView::from_scrape(&scrape).unwrap();
        assert_eq!(view.families.len(), FAMILIES.len());
        assert_eq!(view.families[0].rows[0].value, 2);
        assert_eq!(view.families[0].rows[0].route.as_deref(), Some("a"));
        assert_eq!(view.families[0].rows[0].labels, "result=match");
        // A process-wide series carries no route label, and none is invented.
        let enqueued = view
            .families
            .iter()
            .find(|f| f.name == DIFF_SINK_ENQUEUED_TOTAL)
            .unwrap();
        assert_eq!(enqueued.rows[0].route, None);
        // The skip reasons L1 added are visible here by construction.
        let skipped = view
            .families
            .iter()
            .find(|f| f.name == COMPARISON_SKIPPED_TOTAL)
            .unwrap();
        assert_eq!(skipped.rows[0].labels, "reason=event_stream");
    }

    /// The rollout families are read by their *own* view, and none of them may
    /// join [`FAMILIES`]: three of them are floats (a percentage, a staleness
    /// in seconds), and that array's contract is an exact `u64` per sample.
    /// Widening it to carry them would loosen the one property it exists for —
    /// so the two readers stay disjoint by construction, and this pins it.
    #[test]
    fn the_rollout_families_are_not_on_the_exact_counter_contract() {
        let counters: BTreeSet<&str> = FAMILIES.iter().map(|(name, _)| *name).collect();
        for family in ROLLOUT_FAMILIES {
            assert!(
                !counters.contains(family),
                "{family} is on the exact-u64 counters contract"
            );
        }
        // `limen_requests_total` is read by this section and by neither
        // `FAMILIES` nor `verdict` — stated so a later addition to that array
        // is a deliberate coupling rather than an accident.
        assert!(!counters.contains(REQUESTS_TOTAL));
    }

    /// One `percentage_split` route with a consulted breaker, as the config
    /// side of the rollout section sees it.
    fn split_route() -> ConfigRoute {
        ConfigRoute {
            id: "split".to_string(),
            mode: RouteMode::PercentageSplit,
            comparison_enabled: false,
            sample_rate: 0.0,
            floor: 0,
            expects_floor_row: false,
            rollout: Some(RolloutSettings {
                percentage_flag: "f".to_string(),
                default_percentage: 10.0,
                assignment_header: None,
                assignment_fallback: "request_random".to_string(),
            }),
            breaker: BreakerSettings {
                enabled: true,
                failure_rate_threshold: 0.5,
                min_requests: 20,
                open_duration_ms: 30_000,
                half_open_max_requests: 5,
            },
            failover_safe: false,
        }
    }

    fn scrape(text: &str) -> Scrape {
        Scrape::parse(text).expect("test scrape")
    }

    /// Every rollout series a healthy `split` route exports.
    const ROLLOUT: &str = "\
limen_rollout_resolved_target_percentage{route=\"split\"} 12.5
limen_circuit_breaker_state{route=\"split\",upstream=\"new\"} 2
limen_breaker_transitions_total{route=\"split\",from=\"closed\",to=\"open\"} 1
limen_breaker_transitions_total{route=\"split\",from=\"open\",to=\"half_open\"} 0
limen_breaker_transitions_total{route=\"split\",from=\"half_open\",to=\"closed\"} 0
limen_breaker_transitions_total{route=\"split\",from=\"half_open\",to=\"open\"} 0
limen_flag_provider_stale 0
limen_flag_provider_staleness_seconds 4
limen_flag_provider_consecutive_failures 0
";

    /// The TTL [`ROLLOUT`]'s flag tuple is coherent against — the config
    /// default, so the fixture reads like a real deployment's scrape.
    const TTL_MS: Option<u64> = Some(30_000);

    fn row_of(text: &str) -> RolloutRow {
        let scraped = scrape(text);
        let flags = read_flags(&scraped, TTL_MS);
        rollout_row(&scraped, &split_route(), &flags)
    }

    #[test]
    fn a_whole_rollout_route_reads_off_one_scrape() {
        let row = row_of(ROLLOUT);
        assert!(row.rejected.is_empty(), "{:?}", row.rejected);
        assert_eq!(row.truth.target, Reading::Known(12.5));
        // The gauge says open and the one counted transition agrees.
        assert_eq!(row.truth.breaker_state, Reading::Known(BreakerState::Open));
        assert_eq!(row.truth.transitions, Reading::Known([1, 0, 0, 0]));
        assert_eq!(row.truth.state_skew, None);
        // No request counter at all: both sides absent, and never a 0% share.
        assert_eq!(
            row.truth.observed,
            Reading::Known(ObservedShare {
                new: None,
                legacy: None
            })
        );
    }

    /// The occupancy arithmetic, stated as a table: a breaker starts closed,
    /// and each state is entries minus exits.
    #[test]
    fn the_state_a_transition_history_implies() {
        for (counts, expected) in [
            ([0, 0, 0, 0], BreakerState::Closed),
            ([1, 0, 0, 0], BreakerState::Open),
            ([1, 1, 0, 0], BreakerState::HalfOpen),
            ([1, 1, 1, 0], BreakerState::Closed),
            ([1, 1, 0, 1], BreakerState::Open),
            ([2, 2, 2, 0], BreakerState::Closed),
        ] {
            assert_eq!(state_from_transitions(counts), Ok(expected), "{counts:?}");
        }
        // Tuples no history can produce: more exits than entries, or two
        // entries without an exit between them.
        for impossible in [
            [0, 1, 0, 0], // half-open exited without ever being entered
            [1, 2, 0, 0], // …twice over
            [2, 0, 0, 0], // opened twice without closing
            [0, 0, 1, 0], // closed re-entered without ever leaving
        ] {
            let err = state_from_transitions(impossible).unwrap_err();
            assert!(err.contains("no history a breaker can have"), "{err}");
        }
    }

    /// The counters may run one transition ahead of the gauge (the handler
    /// refreshes the gauge, then renders), never behind it.
    #[test]
    fn only_a_counters_ahead_skew_is_tolerated() {
        use BreakerState::{Closed, HalfOpen, Open};
        assert_eq!(reconcile_state(Closed, Closed), Ok(None));
        assert_eq!(reconcile_state(Closed, Open), Ok(Some(Open)));
        assert_eq!(reconcile_state(Open, HalfOpen), Ok(Some(HalfOpen)));
        assert_eq!(reconcile_state(HalfOpen, Closed), Ok(Some(Closed)));
        assert_eq!(reconcile_state(HalfOpen, Open), Ok(Some(Open)));
        // The only two pairs left, and both are the counters lagging the
        // gauge — impossible, because the counter is incremented under the
        // same lock that stores the state. (The four transitions connect the
        // three states pairwise, so "not one ahead" always means "one behind":
        // there is no third case to word.)
        for (gauge, implied) in [(Open, Closed), (Closed, HalfOpen)] {
            let err = reconcile_state(gauge, implied).unwrap_err();
            assert!(err.contains("never behind it"), "{err}");
            assert!(err.contains("not describing the same breaker"), "{err}");
        }
    }

    /// The tuple table from [`flag_tuple_fault`], exercised on both sides of
    /// the TTL boundary — which is legal in both directions, so a rounding
    /// cannot fail a healthy provider.
    #[test]
    fn the_legal_flag_tuples_are_the_ones_one_snapshot_can_produce() {
        let truth = |stale, age| FlagProviderTruth {
            stale,
            staleness_seconds: age,
            consecutive_failures: 0,
        };
        for legal in [
            truth(true, None),        // never refreshed is always stale
            truth(true, Some(45.0)),  // aged past the TTL
            truth(true, Some(30.0)),  // exactly at it
            truth(false, Some(30.0)), // …legal on the fresh side too
            truth(false, Some(0.0)),
        ] {
            assert_eq!(flag_tuple_fault(&legal, TTL_MS), None, "{legal:?}");
        }
        for (illegal, needle) in [
            (truth(false, None), "never refreshed"),
            (truth(false, Some(45.0)), "past this config's stale_ttl_ms"),
            (truth(true, Some(1.0)), "inside this config's stale_ttl_ms"),
        ] {
            let why = flag_tuple_fault(&illegal, TTL_MS).expect("a fault");
            assert!(why.contains(needle), "{why}");
        }
        // Without a config there is no TTL to check against, and only the
        // contradiction that holds at every TTL can be judged.
        assert!(flag_tuple_fault(&truth(false, None), None).is_some());
        assert_eq!(flag_tuple_fault(&truth(false, Some(45.0)), None), None);
    }

    /// A transition pair limen's state machine cannot make means this is not
    /// limen's breaker being described, so the four counts beside it cannot be
    /// taken as its whole story either.
    #[test]
    fn an_impossible_transition_pair_rejects_the_row() {
        let text = format!(
            "{ROLLOUT}limen_breaker_transitions_total{{route=\"split\",from=\"open\",\
             to=\"closed\"}} 3\n"
        );
        let err = read_transitions(&scrape(&text), "split").unwrap_err();
        assert!(err.contains("open→closed"), "{err}");
        assert!(err.contains("not a transition"), "{err}");
    }

    #[test]
    fn a_target_outside_the_resolvers_range_is_refused() {
        let text = ROLLOUT.replace(
            "limen_rollout_resolved_target_percentage{route=\"split\"} 12.5",
            "limen_rollout_resolved_target_percentage{route=\"split\"} 250",
        );
        let err = read_target(&scrape(&text), "split").unwrap_err();
        assert!(err.contains("250"), "{err}");
        assert!(err.contains("0..=100"), "{err}");

        // …and so is a value that is not a number at all, or two of them.
        for bad in ["NaN", "+Inf"] {
            let text = ROLLOUT.replace("12.5", bad);
            assert!(read_target(&scrape(&text), "split").is_err(), "{bad}");
        }
        let text =
            format!("{ROLLOUT}limen_rollout_resolved_target_percentage{{route=\"split\"}} 30\n");
        let err = read_target(&scrape(&text), "split").unwrap_err();
        assert!(err.contains("more than one"), "{err}");
    }

    /// `-1` is the exporter's sentinel for "no successful refresh, ever" — it
    /// is the one negative reading, and it must not render as an age.
    #[test]
    fn the_staleness_sentinel_is_not_an_age() {
        let text = ROLLOUT.replace(
            "limen_flag_provider_staleness_seconds 4",
            "limen_flag_provider_staleness_seconds -1",
        );
        // The sentinel is read as "never", and the tuple stays coherent only
        // because the fixture's stale gauge is flipped with it.
        let stale_sentinel =
            text.replace("limen_flag_provider_stale 0", "limen_flag_provider_stale 1");
        assert_eq!(
            read_flags(&scrape(&stale_sentinel), TTL_MS),
            FlagReading::Known(FlagProviderTruth {
                stale: true,
                staleness_seconds: None,
                consecutive_failures: 0,
            })
        );
        // Fresh beside the sentinel is the contradiction, not a reading.
        assert!(matches!(
            read_flags(&scrape(&text), TTL_MS),
            FlagReading::Contradiction(_)
        ));

        let text = ROLLOUT.replace(
            "limen_flag_provider_staleness_seconds 4",
            "limen_flag_provider_staleness_seconds -7",
        );
        let FlagReading::Absent(why) = read_flags(&scrape(&text), TTL_MS) else {
            panic!("a negative age that is not the sentinel must not read as one");
        };
        assert!(why.contains("-1 sentinel"), "{why}");
    }

    /// The state gauge is written on every scrape for every route that has a
    /// breaker, so its absence rejects the row: a breaker whose state cannot
    /// be read is exactly the one that must not render as a quiet "closed".
    #[test]
    fn an_absent_breaker_state_rejects_the_row() {
        let text: String = ROLLOUT
            .lines()
            .filter(|l| !l.starts_with(CIRCUIT_BREAKER_STATE))
            .map(|l| format!("{l}\n"))
            .collect();
        let row = row_of(&text);
        assert!(
            row.rejected
                .iter()
                .any(|why| why.contains("never a closed one")),
            "{:?}",
            row.rejected
        );
        assert!(matches!(row.truth.breaker_state, Reading::Unknown(_)));
    }

    /// The breaker guards the new upstream; a state under any other label is
    /// not this breaker's, and reading it would be answering with somebody
    /// else's number.
    #[test]
    fn a_breaker_state_under_another_upstream_is_refused() {
        for label in ["upstream=\"legacy\"", "upstream=\"old\"", "route=\"split\""] {
            let text = ROLLOUT.replace(
                "limen_circuit_breaker_state{route=\"split\",upstream=\"new\"}",
                &format!("limen_circuit_breaker_state{{route=\"split\",{label}}}"),
            );
            let err = read_breaker_state(&scrape(&text), "split").unwrap_err();
            assert!(err.contains("guards the new upstream"), "{label}: {err}");
        }
    }

    #[test]
    fn the_observed_share_is_the_two_counters_and_nothing_else() {
        let text = format!(
            "{ROLLOUT}\
limen_requests_total{{route=\"split\",method=\"GET\",upstream=\"new\",status_class=\"2xx\"}} 1
limen_requests_total{{route=\"split\",method=\"POST\",upstream=\"new\",status_class=\"5xx\"}} 2
limen_requests_total{{route=\"split\",method=\"GET\",upstream=\"legacy\",status_class=\"2xx\"}} 7
limen_requests_total{{route=\"other\",method=\"GET\",upstream=\"new\",status_class=\"2xx\"}} 99
"
        );
        let share = |new, legacy| ObservedShare { new, legacy };
        assert_eq!(
            read_observed(&scrape(&text), "split").unwrap(),
            Reading::Known(share(Some(3), Some(7)))
        );
        assert_eq!(share(Some(3), Some(7)).percentage(), Some(30.0));
        // A route that served nothing on either side has no share — not 0%.
        assert_eq!(share(Some(0), Some(0)).percentage(), None);
        assert_eq!(share(None, None).percentage(), None);
        // An absent side stands for the zero it *is* — but is carried as an
        // absence all the way to the cell, which annotates it rather than
        // printing a bare percentage.
        assert_eq!(share(None, Some(7)).percentage(), Some(0.0));
        assert_eq!(share(None, Some(7)).missing_sides(), ["new"]);
        assert_eq!(share(Some(7), None).percentage(), Some(100.0));
        // The denominator is `u128`: two saturated counters have a share, not
        // an overflow.
        assert_eq!(
            share(Some(u64::MAX), Some(u64::MAX)).percentage(),
            Some(50.0)
        );
    }

    #[test]
    fn the_page_renders_no_script_and_no_external_reference() {
        let dir = tempfile::tempdir().unwrap();
        let html = render_report(&Inputs {
            sink_dir: dir.path().to_path_buf(),
            config: None,
            verdict: None,
            profile: None,
            metrics: None,
        });
        for forbidden in [
            "<script", "src=", "href=", "@import", "url(", "<iframe", "<link", "onerror", "onload",
            "http://", "https://",
        ] {
            assert!(!html.contains(forbidden), "page contains {forbidden:?}");
        }
        assert!(html.contains("INCOMPLETE"), "{html}");
    }
}
