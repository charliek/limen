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
use crate::config::model::Config;
use crate::observability::prometheus::{
    COMPARISONS_TOTAL, COMPARISON_SKIPPED_TOTAL, DIFF_SINK_DROPPED_TOTAL, DIFF_SINK_ENQUEUED_TOTAL,
    DIFF_SINK_WRITTEN_TOTAL, SHADOW_FAILED_TOTAL, SHADOW_IN_FLIGHT, SHADOW_SKIPPED_TOTAL,
    SHADOW_TOTAL,
};
use crate::observability::sink::{self, Report, ReportFilter, REPORT_EXAMPLES_PER_ROUTE};
use crate::verdict::{Scrape, CANARY_ROUTE_ID, RESERVED_ROUTE_ID_PREFIX};

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
}

/// One configured route, as the page renders it.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigRoute {
    pub id: String,
    pub mode: String,
    pub comparison_enabled: bool,
    pub sample_rate: f64,
    /// `comparison.effective_min_comparisons()`.
    pub floor: u64,
    /// Whether `evaluate_floors` would include this route — comparison enabled
    /// *and* a non-zero effective floor. Mirrors that filter so the page can
    /// tell a route the verdict legitimately omits from one it lost.
    pub expects_floor_row: bool,
}

impl ConfigView {
    fn from_config(config: &Config) -> ConfigView {
        ConfigView {
            routes: config
                .routes
                .iter()
                .map(|r| ConfigRoute {
                    id: r.id.clone(),
                    mode: r.mode.as_str().to_string(),
                    comparison_enabled: r.comparison.enabled,
                    sample_rate: r.comparison.sample_rate,
                    floor: r.comparison.effective_min_comparisons(),
                    expects_floor_row: r.comparison.enabled
                        && r.comparison.effective_min_comparisons() > 0,
                })
                .collect(),
        }
    }

    fn route(&self, id: &str) -> Option<&ConfigRoute> {
        self.routes.iter().find(|r| r.id == id)
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

fn read_metrics(path: Option<&PathBuf>) -> Section<MetricsView> {
    let Some(path) = path else {
        return Section::NotProvided;
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => return Section::Unavailable(format!("cannot read metrics artifact: {e}")),
    };
    let scrape = match Scrape::parse(&text) {
        Ok(scrape) => scrape,
        Err(e) => return Section::Unavailable(format!("metrics artifact is not a scrape: {e}")),
    };
    match MetricsView::from_scrape(&scrape) {
        Ok(view) => Section::Ok(view),
        Err(e) => Section::Unavailable(e),
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
    let metrics = read_metrics(inputs.metrics.as_ref());

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
                    esc(&route.mode),
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

fn render_mismatches(out: &mut String, model: &PageModel) {
    out.push_str("<h2>5. Mismatches</h2>\n");
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
    out.push_str("<h2>6. Runtime counters</h2>\n");
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
    out.push_str("<h2>7. Observe profile</h2>\n");
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
