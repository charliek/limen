//! `limen suggest-routes`: turn an observe-mode profile into a draft config.
//!
//! Three steps, in this order: acquire a profile (from the live control plane
//! or a saved file), classify every configured route through
//! [`crate::suggest`], and render a draft the operator reads before they run
//! it. The classification itself lives in [`crate::suggest`] and stays pure —
//! this module is the I/O and the rendering, deliberately separated so the
//! rules can be falsified without a server and the emission can be proven
//! *loadable* without a profile.
//!
//! **The default draft does not shadow anything.** Every route is emitted with
//! `comparison.enabled: false` and its disposition, reason and evidence in a
//! comment block above it. `--adopt-suggestions` is what emits the shadowing
//! form, and it is the mechanical expression of the classifier's epistemic
//! limit ([`crate::suggest`]'s module docs): response metadata can prove a
//! route unsafe to compare but never safe, so no traffic shape may cause this
//! tool to *emit* a config that shadows a mutating read. Promotion is a
//! deliberate human act against the service's source.
//!
//! Two rendering rules are load-bearing rather than cosmetic:
//!
//! - **The whole input document is carried forward**, not a hand-listed subset.
//!   The draft is the input [`Config`] with its `routes` replaced, serialized
//!   through the same serde model the loader reads — so a top-level block
//!   added to the model later cannot silently vanish from a draft. Dropping
//!   `flags`, say, would revert an operator to the `static` provider: a
//!   behavior change wearing a formatting change's clothes.
//! - **A route's `comparison` block is replaced wholesale, never edited.**
//!   Carrying `shadow_methods` or a positive `min_comparisons` onto a
//!   comparison-disabled route is a startup-refusing validation error
//!   (`validate::validate_shadow_methods`,
//!   `validate::validate_comparison_operational`), and a consumer's
//!   hand-written config has exactly that shape. Likewise `contract` is
//!   dropped whenever inline narrowing is emitted, because a contract
//!   reference alongside inline behavioral rules is also a validation error.
//!
//! Exit codes are **this command's own vocabulary**, not `verdict`'s: there is
//! no accumulation here and no "highest wins" rule, and verdict's 20/40 mean
//! floors-unmet / pipeline-never-quiesced against a comparison pipeline that
//! this command does not touch. 0 draft emitted · 20 nothing was profiled ·
//! 40 the profile never quiesced · 50 a required input was unavailable.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::Instant;

use crate::config::model::{ComparisonConfig, Config, RouteConfig, RouteMode};
use crate::observability::observe::{ObserveProfile, RouteProfile, OBSERVE_PROFILE_PATH};
use crate::observability::prometheus::IN_FLIGHT;
use crate::routing::matcher::{basis_normalizes_paths, PathMatcher};
use crate::suggest::{self, Disposition, Evidence, Reason, SuggestThresholds, Suggestion};
use crate::verdict::Scrape;

/// Exit code: a draft was emitted.
pub const EXIT_OK: u8 = 0;
/// Exit code: nothing was profiled, so the draft rests on no evidence.
pub const EXIT_NOTHING_PROFILED: u8 = 20;
/// Exit code: the profile never stopped changing within the deadline.
pub const EXIT_NEVER_QUIESCED: u8 = 40;
/// Exit code: a required input was unavailable or unusable.
pub const EXIT_INPUT_UNAVAILABLE: u8 = 50;

/// Default bound on the quiescence poll.
pub const DEFAULT_DRAIN_DEADLINE_MS: u64 = 2_000;
/// Default interval between quiescence polls.
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 250;

/// How wide a generated comment line may get before it wraps.
const COMMENT_WIDTH: usize = 76;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// The two typed failures that are not a draft. Everything else (an unreadable
/// config, a serializer failure) is an ordinary tooling error and exits 1.
#[derive(Debug, thiserror::Error)]
pub enum SuggestError {
    /// A required input was unavailable: the control plane was unreachable, the
    /// running proxy has no `observe:` block, or a `--profile` file could not be
    /// read or parsed.
    #[error("{0}")]
    InputUnavailable(String),
    /// The profile was still changing (or requests were still in flight) when
    /// the deadline passed. Never downgraded to "good enough": a profile read
    /// mid-flight describes a subset of the traffic that was driven, and a
    /// classification of a subset is not a smaller answer but a possibly wrong
    /// one.
    #[error("{0}")]
    NeverQuiesced(String),
}

impl SuggestError {
    /// The documented process exit code for this failure.
    pub fn exit_code(&self) -> u8 {
        match self {
            SuggestError::InputUnavailable(_) => EXIT_INPUT_UNAVAILABLE,
            SuggestError::NeverQuiesced(_) => EXIT_NEVER_QUIESCED,
        }
    }

    /// The stable machine-readable name used in JSON output.
    pub fn name(&self) -> &'static str {
        match self {
            SuggestError::InputUnavailable(_) => "input-unavailable",
            SuggestError::NeverQuiesced(_) => "never-quiesced",
        }
    }
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Where the profile comes from.
#[derive(Debug, Clone)]
pub enum ProfileSource {
    /// Poll a running proxy's control plane until it quiesces.
    ControlPlane {
        /// Base URL, e.g. `http://127.0.0.1:9090`.
        base: String,
        /// Path the Prometheus exposition is served on (from `metrics.path`).
        metrics_path: String,
    },
    /// Read a saved profile document — the same JSON the endpoint serves, so a
    /// captured profile classifies identically to a live one. No quiescence
    /// poll: a file is already static, and polling it would only measure the
    /// filesystem.
    File(PathBuf),
}

/// Resolved inputs for acquiring and classifying a profile.
///
/// Deliberately **not** a [`SuggestThresholds`]: the third threshold is the
/// sample rate, and that one is not the caller's to state. It comes off the
/// profile document, which the proxy that did the sampling wrote.
#[derive(Debug, Clone)]
pub struct SuggestOptions {
    /// Where the profile comes from.
    pub source: ProfileSource,
    /// R3: reads below this and a route is not classified.
    pub min_samples: u64,
    /// R7: distinct read paths above this and a route is a wildcard proxy.
    pub max_compare_paths: u64,
    /// How long the quiescence poll may wait (control plane only).
    pub drain_deadline: Duration,
    /// Interval between quiescence polls (control plane only).
    pub poll_interval: Duration,
}

/// Resolved inputs for rendering a draft. Separate from [`SuggestOptions`]
/// because emission is a pure function of (config, suggestions, these) — which
/// is what lets the "the draft loads and validates" test run without a profile
/// source at all.
#[derive(Debug, Clone, Default)]
pub struct DraftOptions {
    /// Fallback `new_upstream` for routes that do not configure one.
    pub new_upstream: Option<String>,
    /// Emit the shadowing form for candidates and narrowed routes.
    pub adopt: bool,
    /// The input config's directory — what its relative contract references
    /// resolve against, and therefore what they must be rewritten from so the
    /// draft loads wherever it is written. See [`absolutize`].
    pub base_dir: PathBuf,
}

/// What one run produced.
#[derive(Debug, Clone)]
pub struct SuggestOutcome {
    /// One suggestion per configured route, in configuration order.
    pub suggestions: Vec<Suggestion>,
    /// `0` or `20` — see the module docs.
    pub exit_code: u8,
    /// Operator-facing warnings, printed to stderr so stdout stays a clean
    /// document.
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// Acquire a profile, classify every configured route, and decide the exit
/// code. Rendering is the caller's next step.
pub async fn run_suggest_routes(
    config: &Config,
    opts: &SuggestOptions,
) -> Result<SuggestOutcome, SuggestError> {
    let profile = match &opts.source {
        ProfileSource::File(path) => load_profile_file(path)?,
        ProfileSource::ControlPlane { base, metrics_path } => {
            fetch_quiesced_profile(base, metrics_path, opts).await?
        }
    };
    check_config_describes_the_profiled_proxy(config, &profile)?;
    Ok(evaluate(config, &profile, opts))
}

/// The config supplies the route table; the profile supplies the sample rate
/// and the matcher each route was profiled under. This is where the two are
/// made to agree.
///
/// The rate is not read from the config at all (see
/// [`ObserveProfile::sample_rate`]), but the config still has to be *the
/// profiled proxy's* config, because every other input — the route ids, the
/// path prefixes R1 reads, the `match.query_present` names R6 reads — comes
/// from it and none of them can be corroborated against the profile either. A
/// config that declares no `observe:` block, or one whose rate contradicts the
/// document, is demonstrably not that config: the first describes a proxy whose
/// profile endpoint would 404, and the second a proxy that sampled differently.
/// Both are a required input being unavailable, not a discrepancy to average
/// over.
fn check_config_describes_the_profiled_proxy(
    config: &Config,
    profile: &ObserveProfile,
) -> Result<(), SuggestError> {
    check_profile_consistency(profile)?;
    let Some(observe) = config.observe.as_ref() else {
        return Err(SuggestError::InputUnavailable(format!(
            "the configuration declares no `observe:` block, so it is not the configuration this \
             profile was recorded under (the profile states sample_rate {}) — point -c at the \
             profiled proxy's configuration",
            profile.sample_rate
        )));
    };
    // NaN-tolerant: an unknown rate on either side must still compare equal to
    // itself, and R0 refuses a non-finite rate anyway.
    let agrees = observe.sample_rate == profile.sample_rate
        || (observe.sample_rate.is_nan() && profile.sample_rate.is_nan());
    if !agrees {
        return Err(SuggestError::InputUnavailable(format!(
            "the profile was recorded at observe.sample_rate {} but the configuration declares \
             {} — this is not the configuration the profiled proxy is running, so its route \
             table cannot be trusted to describe the traffic",
            profile.sample_rate, observe.sample_rate
        )));
    }
    check_match_bases(config, profile)
}

/// Refuse a profile whose counters could not have come from the recorder
/// (codex review, C3). The recorder only accrues stability evidence from
/// upstream `2xx` reads, and only counts read transport errors among reads —
/// so a document violating either arithmetic is corrupt or hand-edited, and
/// classifying it would decide off numbers with no recorded meaning. The live
/// path satisfies both by construction; this guards the `--profile` door.
///
/// `pub(crate)` so the classifier's structural sweep can prove that every shape
/// it enumerates is one this door would admit: a sweep asserting invariants over
/// documents the command refuses is a sweep of shapes nobody can send.
pub(crate) fn check_profile_consistency(profile: &ObserveProfile) -> Result<(), SuggestError> {
    for (id, route) in &profile.routes {
        if route.read_transport_errors > route.reads {
            return Err(SuggestError::InputUnavailable(format!(
                "route {id:?}: read_transport_errors ({}) exceeds reads ({}) — the recorder \
                 counts read transport errors among reads, so this profile was not produced \
                 by it. Re-profile",
                route.read_transport_errors, route.reads
            )));
        }
        // `length_varied` counts a SUBSET of `length_repeats` — the recorder
        // bumps both on a repeat whose length moved — so it is not a third
        // disjoint bucket to add in. Only the two disjoint ones consume a
        // successful read each.
        let accounted = route.length_repeats + route.length_missing;
        let successes = crate::suggest::successful_reads(route);
        if accounted > successes {
            return Err(SuggestError::InputUnavailable(format!(
                "route {id:?}: {} repeats + {} reads without a length exceed the {} successful \
                 reads that could have produced them — stability evidence accrues only from 2xx \
                 reads, and each such read is counted once, so this profile was not produced by \
                 the recorder. Re-profile",
                route.length_repeats, route.length_missing, successes
            )));
        }
        if route.length_varied > route.length_repeats {
            return Err(SuggestError::InputUnavailable(format!(
                "route {id:?}: {} varied exceeds {} repeats — a length can only be seen to vary \
                 on a repeat, and the recorder counts the repeat too, so this profile was not \
                 produced by it. Re-profile",
                route.length_varied, route.length_repeats
            )));
        }
    }
    Ok(())
}

/// Refuse a profile whose routes were matched by a different expression than
/// this config declares.
///
/// The same argument as the sample rate, one level down. `distinct_read_paths`
/// counts *paths* under a `path_prefix` route and *shapes* under a
/// `path_template` one, so R7 and R8 read the identical number to opposite
/// conclusions depending on a fact the number itself does not carry. A profile
/// recorded before a route was templated therefore classifies as if the
/// operator had never split it — silently, and in the unsafe direction: the
/// per-id path spread that R7/R8 exist to catch reads as one tidy endpoint. It
/// is not a discrepancy to average over but the wrong input, so the run stops.
///
/// Checked per route and only where both sides have one: a config route the
/// profile never carried is classified against a zero-filled profile (landing
/// on `no-observations`), and a profile id this config does not define is
/// already reported as a warning by [`evaluate`].
fn check_match_bases(config: &Config, profile: &ObserveProfile) -> Result<(), SuggestError> {
    // A profile that shares NO route id with the configuration is some other
    // proxy's profile; skipping every per-route comparison must not read as
    // agreement (absence is never evidence). Partial overlap stays legal —
    // routes added or removed between capture and this run are a normal
    // workflow, and each unmatched side is handled on its own terms.
    let any_shared = config
        .routes
        .iter()
        .any(|r| profile.routes.contains_key(&r.id));
    if !config.routes.is_empty() && !profile.routes.is_empty() && !any_shared {
        return Err(SuggestError::InputUnavailable(format!(
            "the profile's routes ({}) share no id with this configuration's routes — this is \
             not a profile of the configured proxy, so none of its counts describe these \
             routes. Point --profile/-c at a matching pair",
            profile
                .routes
                .keys()
                .map(|k| format!("{k:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    for route in &config.routes {
        let Some(observed) = profile.routes.get(&route.id) else {
            continue;
        };
        // Compiled through the same code the proxy compiled its table with, so
        // the two bases cannot disagree over spelling — only over substance.
        let configured = PathMatcher::compile(&route.id, &route.r#match)
            .map_err(|e| {
                SuggestError::InputUnavailable(format!(
                    "route {:?} has no usable match expression: {e}",
                    route.id
                ))
            })?
            .basis();
        if configured != observed.match_basis {
            return Err(SuggestError::InputUnavailable(format!(
                "route {:?} was profiled as {:?} but this configuration declares {:?} — the \
                 profile's path counts were recorded against a matcher this config no longer \
                 uses, so they do not mean what the classifier would read them as. Re-profile \
                 against this configuration",
                route.id, observed.match_basis, configured
            )));
        }
    }
    Ok(())
}

/// The pure half of a run: classification plus the exit-code decision.
fn evaluate(config: &Config, profile: &ObserveProfile, opts: &SuggestOptions) -> SuggestOutcome {
    let thresholds = SuggestThresholds {
        min_samples: opts.min_samples,
        max_compare_paths: opts.max_compare_paths,
        // From the document, never from the config: R0 is a safety rule, and a
        // rule keyed on a value the operator can hand-edit is not one.
        sample_rate: profile.sample_rate,
    };
    let suggestions = classify_all(config, profile, &thresholds);
    let mut warnings = Vec::new();

    // A profile carrying route ids this config has never heard of is a profile
    // from a different config (or a config edited since the proxy started).
    // The draft is still emitted — it describes *this* config — but the
    // mismatch means some of its routes were classified against nothing.
    let unknown: Vec<&str> = profile
        .routes
        .keys()
        .filter(|id| !config.routes.iter().any(|r| &r.id == *id))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        warnings.push(format!(
            "the profile carries {} route id(s) this config does not define ({}) — it may have \
             been recorded against a different configuration",
            unknown.len(),
            unknown.join(", ")
        ));
    }

    // Exit 20 is the floors doctrine applied to suggestions: a draft nobody's
    // traffic informed is not evidence. Three reasons collapse to the same
    // worthless draft: a config with no routes at all, a config whose every
    // route landed on "we saw nothing" or "we saw too little", and a config
    // whose every route was sampled rather than fully observed — R0 already
    // refuses to classify a sampled profile (relay_only/partial-sample on
    // every route), and a draft resting entirely on that refusal was traffic
    // nobody's evidence informed just as surely as no traffic at all. Exiting
    // 0 there previously let automation read a sampled run as a successful
    // classification.
    let observations: u64 = suggestions.iter().map(|s| s.evidence.observations).sum();
    let unprofiled = suggestions.iter().all(|s| {
        matches!(
            s.reason,
            Reason::NoObservations | Reason::InsufficientReads | Reason::PartialSample
        )
    });
    let exit_code = if observations == 0 || unprofiled {
        warnings.push(
            "nothing was profiled: every route either cleared no traffic, fell below the read \
             floor, or was only sampled (observe.sample_rate < 1.0) — this draft rests on no \
             evidence at all. Drive full, unsampled traffic through the proxy and re-run"
                .to_string(),
        );
        EXIT_NOTHING_PROFILED
    } else {
        EXIT_OK
    };

    // A draft in which nothing reached candidacy is a legitimate answer (an
    // all-`catch-all` config, say), but it is not the answer an operator
    // running this command expects, so it is said out loud rather than left to
    // be inferred from the absence of a word in the output.
    if !suggestions
        .iter()
        .any(|s| s.disposition == Disposition::CompareCandidate)
    {
        warnings.push(
            "no route reached compare_candidate — nothing in this profile supports enabling \
             comparison anywhere"
                .to_string(),
        );
    }

    SuggestOutcome {
        suggestions,
        exit_code,
        warnings,
    }
}

/// Classify every configured route, in configuration order.
///
/// The *config* is the authority on which routes exist: a route the profile
/// has never heard of is classified against a zero-filled profile (landing on
/// `no-observations`) rather than skipped, because a route missing from a
/// draft config is a route that stops being proxied.
pub fn classify_all(
    config: &Config,
    profile: &ObserveProfile,
    thresholds: &SuggestThresholds,
) -> Vec<Suggestion> {
    let unobserved = RouteProfile::default();
    config
        .routes
        .iter()
        .map(|route| {
            let observed = profile.routes.get(&route.id).unwrap_or(&unobserved);
            suggest::classify(route, observed, thresholds)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Profile acquisition
// ---------------------------------------------------------------------------

fn load_profile_file(path: &Path) -> Result<ObserveProfile, SuggestError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        SuggestError::InputUnavailable(format!("cannot read profile {}: {e}", path.display()))
    })?;
    serde_json::from_str(&text).map_err(|e| {
        SuggestError::InputUnavailable(format!(
            "{} is not an observe profile document: {e}",
            path.display()
        ))
    })
}

/// Poll the control plane until the profile has stopped changing **and** no
/// request is in flight, then return it.
///
/// Mirrors `verdict::drain()`'s contract, and for the same reason: two polls
/// 250 ms apart can both be byte-identical while a slow request is still in
/// flight and unrecorded, so identity alone would let a run classify a profile
/// that was about to grow. There are no blind sleeps here — quiescence is
/// observed or the run fails with exit 40.
///
/// **The metrics scrape precedes the profile fetch**, which is the ordering the
/// seam's placement demands: `http::proxy::handle` records its observation
/// *before* dropping the in-flight guard, so `in_flight == 0` at time T implies
/// every request that finished before T is already in the profile — and a
/// profile read after that reading therefore cannot be missing one. Read in the
/// other order, a request could finish and record in the gap, and the run would
/// return a profile it had already declared complete.
async fn fetch_quiesced_profile(
    base: &str,
    metrics_path: &str,
    opts: &SuggestOptions,
) -> Result<ObserveProfile, SuggestError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| SuggestError::InputUnavailable(format!("cannot build HTTP client: {e}")))?;
    let metrics_url = format!("{base}{metrics_path}");
    let profile_url = format!("{base}{OBSERVE_PROFILE_PATH}");

    let deadline = Instant::now() + opts.drain_deadline;
    let mut previous: Option<String> = None;
    let mut in_flight;
    loop {
        in_flight = scrape_in_flight(&client, &metrics_url).await?;
        let document = fetch_profile(&client, &profile_url).await?;
        // Byte-identical, not semantically equal: the profile is canonically
        // serialized (BTreeMap ordering, no wall-clock field), so equal bytes
        // is exactly "nothing was recorded between these two reads".
        if in_flight == 0.0 && previous.as_deref() == Some(document.as_str()) {
            return parse_profile(&document, &profile_url);
        }
        previous = Some(document);
        if Instant::now() >= deadline {
            return Err(SuggestError::NeverQuiesced(format!(
                "the profile never quiesced within {} ms: {in_flight} request(s) in flight at the \
                 last poll and the document was still changing — stop driving traffic before \
                 running suggest-routes, or raise --drain-deadline-ms",
                opts.drain_deadline.as_millis()
            )));
        }
        tokio::time::sleep(opts.poll_interval).await;
    }
}

/// Read `limen_in_flight_requests` from one scrape.
///
/// An absent series is a typed failure, never a zero: the gauge is
/// zero-registered at startup, so absence means the scrape hit something that
/// is not a limen control plane (or a binary older than observe mode) — and
/// reading it as "nothing in flight" would turn that into a confidently wrong
/// draft.
async fn scrape_in_flight(client: &reqwest::Client, url: &str) -> Result<f64, SuggestError> {
    let text = get_text(client, url).await?;
    let scrape = Scrape::parse(&text).map_err(|e| SuggestError::InputUnavailable(e.0))?;
    if !scrape.has_family(IN_FLIGHT) {
        return Err(SuggestError::InputUnavailable(format!(
            "required series {IN_FLIGHT} absent from {url} — the proxy is not exporting the \
             in-flight gauge quiescence is measured against (older binary?)"
        )));
    }
    Ok(scrape.sum(IN_FLIGHT, &[]).unwrap_or(0.0))
}

/// Fetch the profile document, translating the endpoint's 404 into the thing it
/// actually means. The route is registered only while observe mode is on
/// (`health::endpoints::router`), so a 404 is "this proxy is not observing",
/// not "wrong URL".
async fn fetch_profile(client: &reqwest::Client, url: &str) -> Result<String, SuggestError> {
    let resp = client.get(url).send().await.map_err(|e| {
        SuggestError::InputUnavailable(format!("control plane unreachable at {url}: {e}"))
    })?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(SuggestError::InputUnavailable(format!(
            "{url} returned 404 — the running proxy has no `observe:` block, so it has profiled \
             nothing. Add the block and restart it before profiling"
        )));
    }
    if !resp.status().is_success() {
        return Err(SuggestError::InputUnavailable(format!(
            "control plane returned HTTP {} for {url}",
            resp.status()
        )));
    }
    resp.text()
        .await
        .map_err(|e| SuggestError::InputUnavailable(format!("cannot read {url}: {e}")))
}

async fn get_text(client: &reqwest::Client, url: &str) -> Result<String, SuggestError> {
    let resp = client.get(url).send().await.map_err(|e| {
        SuggestError::InputUnavailable(format!("control plane unreachable at {url}: {e}"))
    })?;
    if !resp.status().is_success() {
        return Err(SuggestError::InputUnavailable(format!(
            "control plane returned HTTP {} for {url}",
            resp.status()
        )));
    }
    resp.text()
        .await
        .map_err(|e| SuggestError::InputUnavailable(format!("cannot read {url}: {e}")))
}

fn parse_profile(document: &str, url: &str) -> Result<ObserveProfile, SuggestError> {
    serde_json::from_str(document).map_err(|e| {
        SuggestError::InputUnavailable(format!("{url} did not serve an observe profile: {e}"))
    })
}

// ---------------------------------------------------------------------------
// Draft construction
// ---------------------------------------------------------------------------

/// One route's drafted form plus the notes explaining what the draft did to it.
struct DraftedRoute {
    route: RouteConfig,
    notes: Vec<String>,
}

/// Build the drafted form of one route.
///
/// Only the fields that describe *where the request goes and how long it may
/// take* survive verbatim (`id`, `match`, `legacy_upstream`, `timeouts`,
/// `circuit_breaker`, `rollout`, `failover_safe`, `budget`). `mode`,
/// `new_upstream`, `comparison` and `contract` are decided here.
fn draft_route(input: &RouteConfig, suggestion: &Suggestion, opts: &DraftOptions) -> DraftedRoute {
    let mut notes = Vec::new();
    let mut route = input.clone();

    // A contract reference resolves against the directory of the config that
    // carries it, so a draft written anywhere but beside its input would fail
    // to load with the reference untouched. See `absolutize`.
    if let Some(reference) = route.contract.take() {
        route.contract = Some(absolutize_contract_reference(&opts.base_dir, &reference));
    }

    // The comparison block is BUILT, never edited: `shadow_methods` and a
    // positive `min_comparisons` are both startup-refusing on a disabled route,
    // and both are shapes a real config carries.
    route.comparison = ComparisonConfig::default();

    let new_upstream = input
        .new_upstream
        .clone()
        .or_else(|| opts.new_upstream.clone());

    // A route that already serves from `new` is not a route this draft may
    // re-point. Rewriting `new_only` or `percentage_split` to
    // `shadow_legacy_primary` would move live client traffic back to legacy —
    // a behavior change, and a far larger one than the `flags` block this
    // command is careful to carry forward. Such a route keeps its mode and
    // stays uncompared, because comparison is inert outside
    // `shadow_legacy_primary` anyway (`http::shadow::plan`).
    let shadowable = matches!(
        input.mode,
        RouteMode::LegacyOnly | RouteMode::ShadowLegacyPrimary
    ) && input.legacy_upstream.is_some();

    if !shadowable {
        notes.push(format!(
            "left at mode {}: this route does not serve from a legacy primary, and re-pointing \
             it would move live traffic rather than reformat a file. Comparison stays off.",
            input.mode.as_str()
        ));
        return DraftedRoute { route, notes };
    }

    match new_upstream {
        Some(url) => {
            route.new_upstream = Some(url);
            route.mode = RouteMode::ShadowLegacyPrimary;
            route.comparison = comparison_for(suggestion.disposition, opts.adopt);
            // Say how to act on the suggestion, because the obvious manual
            // move — flipping `enabled: true` — is not equivalent to adopting
            // it. On a narrowed route it is actively different: the narrowing
            // this comment describes is emitted only under the flag, so a
            // hand-flipped route compares its body after all, under whatever
            // contract it already references.
            if !opts.adopt {
                match (suggestion.disposition, route.contract.is_some()) {
                    (Disposition::CompareNarrowed, true) => notes.push(
                        "to adopt this, re-run with --adopt-suggestions: it emits compare_status: \
                         true / compare_body: false. Setting comparison.enabled: true by hand \
                         instead leaves this route's contract in force, so the body IS compared — \
                         not the narrowed form described above."
                            .to_string(),
                    ),
                    (Disposition::CompareNarrowed, false) => notes.push(
                        "to adopt this, re-run with --adopt-suggestions: it emits compare_status: \
                         true / compare_body: false. Setting comparison.enabled: true by hand \
                         instead compares the body too, which this route's responses do not \
                         support."
                            .to_string(),
                    ),
                    (Disposition::CompareCandidate, _) => notes.push(
                        "to adopt this, re-run with --adopt-suggestions — once you have confirmed \
                         against the service's source that this route does not mutate."
                            .to_string(),
                    ),
                    (Disposition::RelayOnly, _) => {}
                }
            }
            // contract + inline behavioral rules is a validation error, and the
            // narrowing this command emits is inline by construction. The drop
            // is announced: a contract is a human's authored definition of
            // equality, and losing one silently would be worse than the
            // narrowing is worth.
            if route.comparison.has_inline_behavioral() {
                if let Some(reference) = route.contract.take() {
                    notes.push(format!(
                        "contract {reference:?} dropped: inline narrowing and a contract \
                         reference cannot coexist. Prefer the contract — re-add it and delete \
                         the inline compare_status/compare_body if it already says this.",
                    ));
                }
            }
        }
        None => {
            // Valid whether or not a `new` upstream exists yet: the draft is
            // meant to be read and edited before it is run, and a document that
            // refuses to load is not readable.
            route.new_upstream = None;
            route.mode = RouteMode::LegacyOnly;
            notes.push(
                "no new upstream is configured for this route and none was given: emitted \
                 mode: legacy_only. Pass --new-upstream URL (or fill in new_upstream) to draft \
                 the shadowing form."
                    .to_string(),
            );
        }
    }

    DraftedRoute { route, notes }
}

/// The `comparison` block a disposition earns.
///
/// Under the default (no `--adopt-suggestions`) this is `enabled: false` for
/// every disposition — the suggestion rides as a comment and nothing shadows.
fn comparison_for(disposition: Disposition, adopt: bool) -> ComparisonConfig {
    let mut comparison = ComparisonConfig::default();
    if !adopt {
        return comparison;
    }
    match disposition {
        Disposition::RelayOnly => comparison,
        Disposition::CompareCandidate | Disposition::CompareNarrowed => {
            comparison.enabled = true;
            comparison.sample_rate = 1.0;
            // limen's own default, taken from the model rather than restated,
            // so a consumer's hand-tuned value can never be reverse-engineered
            // into the tool.
            comparison.max_body_bytes = ComparisonConfig::default().max_body_bytes;
            if disposition == Disposition::CompareNarrowed {
                // All four narrowing reasons concern body trustworthiness, so
                // the narrowing is uniformly "compare status, not body". It
                // deliberately does NOT declare `set_cookie` or `location`:
                // those dimensions are OFF in plain comparison
                // (`contract::model::ComparisonRules::default`), and turning
                // one on would make "narrowed" a misnomer for "widened".
                comparison.compare_status = Some(true);
                comparison.compare_body = Some(false);
            }
            comparison
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the machine surface: one object per route, in configuration order.
pub fn render_json(suggestions: &[Suggestion]) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(suggestions)
}

/// Render the draft configuration document.
///
/// `suggestions` must be [`classify_all`]'s output for this same config: one
/// per route, in configuration order.
pub fn render_yaml(
    config: &Config,
    suggestions: &[Suggestion],
    opts: &DraftOptions,
) -> Result<String, serde_yaml::Error> {
    debug_assert_eq!(
        config.routes.len(),
        suggestions.len(),
        "one suggestion per configured route"
    );
    let mut out = String::new();
    out.push_str(&header(opts));

    // Everything above `routes:` is the input document, serialized through the
    // same model the loader reads and with only `routes` removed — so a block
    // this command has never heard of still survives a round trip.
    let mut head = config.clone();
    head.routes.clear();
    // …with one deliberate exception: relative paths are pinned to what they
    // resolved to for the *input*. These three are opened by the runtime
    // directly (`http::client`, `flags::file_provider`, the sink writer), so
    // they are resolved against the process CWD rather than the config's
    // directory — which is exactly why a draft run from a different CWD would
    // otherwise read a different file, or none.
    if head.flags.file.path.as_os_str().is_empty() {
        // An empty path is a validation error the input already failed on; do
        // not turn it into the CWD.
    } else {
        head.flags.file.path = absolutize(Path::new(""), &head.flags.file.path);
    }
    if let Some(ca) = head.upstream_tls.ca_bundle_path.take() {
        head.upstream_tls.ca_bundle_path = Some(absolutize(Path::new(""), &ca));
    }
    if let Some(sink) = head.diff_sink.as_mut() {
        sink.dir = absolutize(Path::new(""), &sink.dir);
    }
    let mut value = serde_yaml::to_value(&head)?;
    if let Some(mapping) = value.as_mapping_mut() {
        mapping.remove("routes");
    }
    out.push_str(&serde_yaml::to_string(&value)?);

    out.push_str("routes:\n");
    for (route, suggestion) in config.routes.iter().zip(suggestions) {
        let drafted = draft_route(route, suggestion, opts);
        for line in comment_block(suggestion, &drafted) {
            if line.is_empty() {
                out.push_str("  #\n");
            } else {
                out.push_str("  # ");
                out.push_str(&line);
                out.push('\n');
            }
        }
        out.push_str(&indent_as_list_item(&serde_yaml::to_string(
            &drafted.route,
        )?));
    }
    Ok(out)
}

/// The document preamble: what this file is and what it is not.
fn header(opts: &DraftOptions) -> String {
    let mut out = String::from(
        "# limen suggest-routes draft — generated from an observe-mode profile.\n\
         #\n\
         # A STARTING POINT, NOT A VERDICT. Observation can prove a route unsafe to\n\
         # compare; it can never prove one safe — nothing observable distinguishes a\n\
         # read from a write wearing a read's clothes. Each SUGGESTED block below\n\
         # carries the evidence its disposition rested on; read it against the\n\
         # service's source before enabling comparison on any route.\n\
         #\n\
         # Relative paths (contract references, flags file, CA bundle, sink dir) have\n\
         # been made absolute: a draft is a working artifact meant to load from\n\
         # wherever it was written, not a config to check in as-is. Re-relativize\n\
         # them before committing this anywhere.\n\
         #\n",
    );
    if opts.adopt {
        out.push_str(
            "# GENERATED WITH --adopt-suggestions: routes suggested compare_candidate or\n\
             # compare_narrowed are emitted with comparison ENABLED, which dispatches a\n\
             # shadow request to the new upstream. That promotion rests on your\n\
             # confirmation against the service's source, not on the profile.\n",
        );
    } else {
        out.push_str(
            "# Nothing here shadows: every route is emitted comparison.enabled: false.\n\
             # Re-run with --adopt-suggestions once you have confirmed each candidate by\n\
             # hand, or enable the routes you have confirmed yourself.\n",
        );
    }
    out.push('\n');
    out
}

/// The comment block above one route: disposition, evidence, caveat, notes.
fn comment_block(suggestion: &Suggestion, drafted: &DraftedRoute) -> Vec<String> {
    let evidence = &suggestion.evidence;
    // The machine-readable reason rides in the headline alongside the prose:
    // it is the published vocabulary a consumer greps for, and a comment that
    // only paraphrased it would make the draft and the `--format json` surface
    // two different documents.
    let mut lines = wrap_words(
        "SUGGESTED: ",
        "  ",
        &format!(
            "{} ({}) — {}",
            suggestion.disposition.as_str(),
            suggestion.reason.as_str(),
            headline(suggestion)
        ),
    );
    lines.extend(wrap(
        "  evidence: ",
        "            ",
        &evidence_items(suggestion),
    ));

    // First-match-wins is right for the decision and wrong for the
    // explanation, so every narrowing rule the reason hid is named too.
    let hidden: Vec<&str> = evidence
        .narrowing_matches
        .iter()
        .filter(|r| **r != suggestion.reason)
        .map(|r| r.as_str())
        .collect();
    if !hidden.is_empty() {
        lines.extend(wrap(
            "  also matched: ",
            "                ",
            &hidden.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(),
        ));
    }

    // The caveat rides on `compare_narrowed` as well as `compare_candidate`:
    // narrowed still enables comparison, and therefore still dispatches a
    // shadow request. Only relay-only routes are exempt, because nothing is
    // sent anywhere on their behalf.
    if suggestion.disposition != Disposition::RelayOnly {
        lines.extend(wrap_words(
            "  ",
            "  ",
            "Observation cannot prove this route does not mutate. Confirm against the service's \
             source before enabling comparison.",
        ));
    }
    for note in &drafted.notes {
        lines.extend(wrap_words("  note: ", "        ", note));
    }
    lines
}

/// The one-line reason, phrased with the numbers that produced it.
fn headline(suggestion: &Suggestion) -> String {
    let e = &suggestion.evidence;
    match suggestion.reason {
        Reason::PartialSample => "the profile was sampled (observe.sample_rate is not 1.0), so \
                                  every existential rule below is unsound"
            .to_string(),
        Reason::CatchAll => {
            "path_prefix \"/\" is the unclassified remainder — comparing it shadows every path \
             nobody has looked at"
                .to_string()
        }
        Reason::NoObservations => "no traffic was observed on this route".to_string(),
        Reason::InsufficientReads => format!(
            "only {} read(s) observed — below the --min-samples floor",
            e.reads
        ),
        Reason::RedirectingRead => format!(
            "{} read(s) answered 3xx and {} carried Location — the shape of a flow hop",
            e.redirect_reads, e.location_reads
        ),
        Reason::MintsState => format!("{} read(s) set a cookie", e.set_cookie_reads),
        Reason::QueryNamesUnrecorded => {
            "the recorded query-parameter names are known to be incomplete".to_string()
        }
        Reason::OneTimeTokenQuery => {
            // Both sources, deduplicated: "traffic carried a verifier" and
            // "this route is *defined* as the verifier hop" are different
            // facts, and a route can present either or both.
            let names: std::collections::BTreeSet<&str> = e
                .one_time_token_names_observed
                .iter()
                .chain(e.one_time_token_names_configured.iter())
                .map(String::as_str)
                .collect();
            format!(
                "one-time-token query parameter(s): {}",
                names.into_iter().collect::<Vec<_>>().join(", ")
            )
        }
        Reason::WildcardGranularity => format!(
            "reads spread over {}{} distinct paths",
            e.distinct_read_paths,
            if e.distinct_read_paths_overflow {
                "+ (the recorder's cap was hit)"
            } else {
                ""
            }
        ),
        Reason::OpaquePathIds => format!(
            "{} distinct paths across {} reads — ids or tokens are in the path",
            e.distinct_read_paths, e.reads
        ),
        Reason::NoSuccessEvidence => format!(
            "no read ever succeeded ({}) — the only bodies observed were failures, so nothing \
             here describes what the route serves",
            status_mix(e)
        ),
        Reason::BodyVaries => format!(
            "{} repeated request(s) came back at a different Content-Length",
            e.length_varied
        ),
        Reason::StabilityUnobserved => format!(
            "{} read(s) carried no Content-Length, so stability was never established",
            e.length_missing
        ),
        Reason::NoRepeatEvidence => if e.fingerprint_overflow {
            "the recorder's fingerprint cap was hit, so variance would go unrecorded"
        } else {
            "no request ever repeated, so nothing was learned about stability"
        }
        .to_string(),
        Reason::ContentTypeVaries => format!(
            "{}{} content types observed",
            e.content_types.len(),
            if e.content_types_overflow { "+" } else { "" }
        ),
        Reason::StableRepeatedReads => format!(
            "stable across {} repeated request(s)",
            suggestion.evidence.length_repeats
        ),
    }
}

/// The read status mix, as the evidence line renders it — and as R8a's headline
/// quotes it, since "no read ever succeeded" is a claim about this map and a
/// reader should not have to scan down to the evidence to see which failures.
fn status_mix(e: &Evidence) -> String {
    if e.status_classes.is_empty() {
        "no read status recorded".to_string()
    } else if e.status_classes.len() == 1 {
        format!(
            "{} only",
            e.status_classes.keys().next().expect("one class")
        )
    } else {
        e.status_classes
            .iter()
            .map(|(class, n)| format!("{class}×{n}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The evidence items, in the order a reader wants them.
fn evidence_items(suggestion: &Suggestion) -> Vec<String> {
    let e = &suggestion.evidence;
    // Said out loud, because "1 path" on a route serving thousands of ids is
    // otherwise the most misreadable number on the page: the template folded
    // them, and the path-spread rules had nothing left to see.
    let normalized = if basis_normalizes_paths(&e.match_basis) {
        " (template-normalized)"
    } else {
        ""
    };
    let mut items = vec![
        format!("{} reads / {} writes", e.reads, e.writes),
        format!(
            "{}{} path{}{normalized}",
            e.distinct_read_paths,
            if e.distinct_read_paths_overflow {
                "+"
            } else {
                ""
            },
            if e.distinct_read_paths == 1 { "" } else { "s" },
        ),
    ];
    items.push(status_mix(e));
    items.push(if e.content_types.is_empty() {
        "no content-type recorded".to_string()
    } else {
        format!(
            "{}{}",
            e.content_types
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" "),
            if e.content_types_overflow {
                " +more"
            } else {
                ""
            }
        )
    });
    items.push(if e.set_cookie_reads == 0 {
        "no Set-Cookie".to_string()
    } else {
        format!("{} Set-Cookie", e.set_cookie_reads)
    });
    items.push(if e.redirect_reads == 0 && e.location_reads == 0 {
        "no redirect".to_string()
    } else {
        format!("{} 3xx / {} Location", e.redirect_reads, e.location_reads)
    });
    items.push(
        match (e.length_repeats, e.length_varied, e.length_missing) {
            (0, _, _) => "no repeated request".to_string(),
            (repeats, 0, 0) => format!("Content-Length stable over {repeats} repeats"),
            (repeats, varied, missing) => {
                format!("{repeats} repeats, {varied} varied, {missing} without a length")
            }
        },
    );
    if e.transport_errors > 0 {
        // Both numbers when they differ: R8a's carve-out is read-scoped, so a
        // reader checking why a route with transport errors still demoted (or
        // still did not) needs to see which of the two the rule looked at.
        items.push(if e.read_transport_errors == e.transport_errors {
            format!("{} transport errors", e.transport_errors)
        } else {
            format!(
                "{} transport errors ({} on reads)",
                e.transport_errors, e.read_transport_errors
            )
        });
    }
    if !e.one_time_token_names_configured.is_empty() {
        items.push(format!(
            "match.query_present: {}",
            e.one_time_token_names_configured
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    items
}

/// Join `items` with `·` separators, wrapping at [`COMMENT_WIDTH`].
fn wrap(first_prefix: &str, continuation: &str, items: &[String]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = first_prefix.to_string();
    let mut empty = true;
    for item in items {
        let candidate = if empty {
            format!("{current}{item}")
        } else {
            format!("{current} · {item}")
        };
        if !empty && candidate.chars().count() > COMMENT_WIDTH {
            lines.push(current);
            current = format!("{continuation}· {item}");
        } else {
            current = candidate;
        }
        empty = false;
    }
    if !empty {
        lines.push(current);
    }
    lines
}

/// Wrap prose on whitespace at [`COMMENT_WIDTH`].
fn wrap_words(first_prefix: &str, continuation: &str, text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = first_prefix.to_string();
    let mut empty = true;
    for word in text.split_whitespace() {
        let candidate = if empty {
            format!("{current}{word}")
        } else {
            format!("{current} {word}")
        };
        if !empty && candidate.chars().count() > COMMENT_WIDTH {
            lines.push(current);
            current = format!("{continuation}{word}");
        } else {
            current = candidate;
        }
        empty = false;
    }
    if !empty {
        lines.push(current);
    }
    lines
}

/// Turn a serialized mapping into a two-space-indented YAML list item.
fn indent_as_list_item(mapping: &str) -> String {
    let mut out = String::new();
    for (index, line) in mapping.lines().enumerate() {
        if index == 0 {
            out.push_str("  - ");
        } else if line.is_empty() {
            out.push('\n');
            continue;
        } else {
            out.push_str("    ");
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Derivations shared with the CLI
// ---------------------------------------------------------------------------

/// Make a path in an emitted draft resolve to the same file the input config's
/// copy of it did, from any working directory.
///
/// A draft is a working artifact — `limen suggest-routes -c config/limen.yaml >
/// /tmp/draft.yaml` is the expected shape of the command — and every relative
/// path in a config is resolved against something the draft's new location
/// changes. Left alone, a relocated draft either fails to load (a contract
/// reference, resolved against the *draft's* directory) or silently reads a
/// different file (a CWD-relative path, run from elsewhere).
///
/// `base` is whatever that field is resolved against: the config's directory
/// for contract references, the process CWD for everything the runtime opens
/// directly. Best-effort — a path that cannot be made absolute is emitted
/// unchanged rather than dropped.
fn absolutize(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let joined = if base.as_os_str().is_empty() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    std::path::absolute(&joined).unwrap_or(joined)
}

/// Rewrite a `path#routeId` contract reference so its path half is absolute,
/// split exactly as [`crate::contract::load`] splits it.
fn absolutize_contract_reference(base_dir: &Path, reference: &str) -> String {
    match reference.rsplit_once('#') {
        Some((file, route_id)) if !file.is_empty() && !route_id.is_empty() => {
            let resolved = absolutize(base_dir, Path::new(file));
            format!("{}#{route_id}", resolved.display())
        }
        // Malformed references are emitted untouched: rewriting one would
        // change a load error's message without fixing anything, and
        // validation of the *input* has already rejected it.
        _ => reference.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::model::ObserveConfig;
    use crate::suggest::{DEFAULT_MAX_COMPARE_PATHS, DEFAULT_MIN_SAMPLES};

    fn config_from(yaml: &str) -> Config {
        serde_yaml::from_str(yaml).expect("valid test config")
    }

    /// Contract references resolve against the config's base directory, so
    /// every draft here is validated as though it sat in the repo's `config/`
    /// directory — the one place a real contract file exists to resolve to.
    fn base_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config")
    }

    /// An unobserved profile, recorded at full rate. Written out rather than
    /// derived from `Default`, which `ObserveProfile` deliberately does not
    /// implement: a defaulted sample rate is a claim about completeness that
    /// only the recorder is entitled to make.
    fn empty_profile() -> ObserveProfile {
        ObserveProfile {
            sample_rate: 1.0,
            routes: BTreeMap::new(),
        }
    }

    /// A profile in which `route_id` looks like a clean, stable read: enough
    /// reads, one path, one content type, repeats at a stable length.
    fn candidate_profile(route_id: &str) -> ObserveProfile {
        let mut profile = empty_profile();
        profile.routes.insert(
            route_id.to_string(),
            RouteProfile {
                observations: 34,
                reads: 34,
                distinct_read_paths: 1,
                status_classes: BTreeMap::from([("2xx".to_string(), 34)]),
                content_types: ["application/json".to_string()].into_iter().collect(),
                length_repeats: 12,
                ..RouteProfile::default()
            },
        );
        profile
    }

    /// The thresholds a run derives: the two caller floors at their defaults,
    /// and the sample rate off the profile itself.
    fn thresholds_of(profile: &ObserveProfile) -> SuggestThresholds {
        SuggestThresholds {
            min_samples: DEFAULT_MIN_SAMPLES,
            max_compare_paths: DEFAULT_MAX_COMPARE_PATHS,
            sample_rate: profile.sample_rate,
        }
    }

    fn opts() -> SuggestOptions {
        SuggestOptions {
            source: ProfileSource::File(PathBuf::from("/dev/null")),
            min_samples: DEFAULT_MIN_SAMPLES,
            max_compare_paths: DEFAULT_MAX_COMPARE_PATHS,
            drain_deadline: Duration::from_millis(DEFAULT_DRAIN_DEADLINE_MS),
            poll_interval: Duration::from_millis(DEFAULT_POLL_INTERVAL_MS),
        }
    }

    /// The shape a real consumer's config has: a comparison block carrying
    /// `shadow_methods` and a floor, plus a contract reference.
    fn hostile_config() -> Config {
        config_from(
            r#"
observe: {}
debug:
  upstream_header: true
flags:
  provider: file
  file: { path: "./flags.local.yaml" }
routes:
  - id: pat-validate
    match: { methods: ["GET", "POST"], path_prefix: "/api/v1/pat/validate" }
    legacy_upstream: "http://legacy.internal"
    new_upstream: "http://new.internal"
    mode: shadow_legacy_primary
    contract: "contracts/example-service.contract.yaml#get-device"
    comparison:
      enabled: true
      sample_rate: 1.0
      max_body_bytes: 1048576
      min_comparisons: 20
      shadow_methods: ["POST"]
"#,
        )
    }

    fn draft_of(config: &Config, profile: &ObserveProfile, opts: &DraftOptions) -> String {
        let suggestions = classify_all(config, profile, &thresholds_of(profile));
        // Every draft here is emitted as though its input config sat in the
        // repo's `config/` directory, so relative contract references
        // absolutize to a file that exists.
        let opts = DraftOptions {
            base_dir: base_dir(),
            ..opts.clone()
        };
        render_yaml(config, &suggestions, &opts).expect("render")
    }

    /// The load-bearing emission test: a draft of a config carrying every shape
    /// validation refuses on a disabled route must still load and validate.
    fn assert_valid(draft: &str) -> Config {
        let config: Config = serde_yaml::from_str(draft)
            .unwrap_or_else(|e| panic!("draft does not parse: {e}\n{draft}"));
        crate::config::validate(&config, &base_dir())
            .unwrap_or_else(|e| panic!("draft does not validate: {e:?}\n{draft}"));
        config
    }

    #[test]
    fn the_default_draft_never_enables_comparison() {
        let config = hostile_config();
        let draft = draft_of(
            &config,
            &candidate_profile("pat-validate"),
            &DraftOptions::default(),
        );
        let parsed = assert_valid(&draft);
        // On the parsed document: the comments legitimately mention
        // `comparison.enabled: true` while explaining what adopting would do.
        assert!(
            parsed.routes.iter().all(|r| !r.comparison.enabled),
            "{draft}"
        );
        // The suggestion still rides as a comment: a draft that shadows nothing
        // and says nothing would be useless rather than safe.
        assert!(draft.contains("SUGGESTED: compare_candidate"), "{draft}");
    }

    #[test]
    fn adopting_a_candidate_enables_comparison_at_limens_own_default() {
        let config = hostile_config();
        let draft = draft_of(
            &config,
            &candidate_profile("pat-validate"),
            &DraftOptions {
                adopt: true,
                ..DraftOptions::default()
            },
        );
        let parsed = assert_valid(&draft);
        let comparison = &parsed.routes[0].comparison;
        assert!(comparison.enabled);
        assert_eq!(comparison.sample_rate, 1.0);
        assert_eq!(comparison.max_body_bytes, 262_144);
        // Not narrowed: a candidate compares status and body.
        assert_eq!(comparison.compare_status, None);
        assert_eq!(comparison.compare_body, None);
    }

    #[test]
    fn the_comparison_block_is_replaced_wholesale_not_edited() {
        // Both of these are startup-refusing on a comparison-disabled route,
        // and both are what an edit-in-place implementation would carry
        // through. `assert_valid` is what actually catches it; the field
        // assertions say why.
        let config = hostile_config();
        let draft = draft_of(&config, &empty_profile(), &DraftOptions::default());
        let parsed = assert_valid(&draft);
        assert!(parsed.routes[0].comparison.shadow_methods.is_empty());
        assert_eq!(parsed.routes[0].comparison.min_comparisons, None);
        assert!(!draft.contains("shadow_methods"), "{draft}");
        assert!(!draft.contains("min_comparisons"), "{draft}");
    }

    #[test]
    fn inline_narrowing_drops_the_contract_reference() {
        let config = hostile_config();
        // One read with no Content-Length is R11: narrowed, so inline rules are
        // emitted — which cannot coexist with the contract reference.
        let mut profile = candidate_profile("pat-validate");
        profile
            .routes
            .get_mut("pat-validate")
            .expect("route")
            .length_missing = 1;
        let draft = draft_of(
            &config,
            &profile,
            &DraftOptions {
                adopt: true,
                ..DraftOptions::default()
            },
        );
        let parsed = assert_valid(&draft);
        assert_eq!(parsed.routes[0].contract, None);
        assert_eq!(parsed.routes[0].comparison.compare_status, Some(true));
        assert_eq!(parsed.routes[0].comparison.compare_body, Some(false));
        // The narrowing must not turn a dimension ON that plain comparison
        // leaves off — that would be a widening wearing narrowing's name.
        assert_eq!(parsed.routes[0].comparison.set_cookie, None);
        assert_eq!(parsed.routes[0].comparison.location, None);
        assert!(draft.contains("dropped"), "{draft}");
    }

    #[test]
    fn without_narrowing_the_contract_reference_survives() {
        let config = hostile_config();
        let draft = draft_of(
            &config,
            &candidate_profile("pat-validate"),
            &DraftOptions {
                adopt: true,
                ..DraftOptions::default()
            },
        );
        let parsed = assert_valid(&draft);
        // Kept, and pinned: the reference resolves against the *config's*
        // directory, which a relocated draft no longer shares.
        let reference = parsed.routes[0].contract.clone().expect("contract kept");
        let (file, route_id) = reference.rsplit_once('#').expect("path#routeId");
        assert!(Path::new(file).is_absolute(), "{reference}");
        assert!(file.ends_with("config/contracts/example-service.contract.yaml"));
        assert_eq!(route_id, "get-device");
    }

    #[test]
    fn the_head_of_the_document_is_carried_forward_with_its_paths_pinned() {
        let config = hostile_config();
        let draft = draft_of(
            &config,
            &candidate_profile("pat-validate"),
            &DraftOptions::default(),
        );
        let parsed = assert_valid(&draft);
        // Dropping `flags` would silently revert the operator to the `static`
        // provider — a behavior change, not a formatting one.
        assert_eq!(parsed.flags.provider, config.flags.provider);
        assert_eq!(parsed.flags.fail_safe_mode, config.flags.fail_safe_mode);
        assert_eq!(parsed.observe, config.observe);
        // `debug` is the same shape as `flags`/`observe`: dropping it would
        // silently turn a running debug affordance off across a
        // suggest-routes round trip.
        assert_eq!(parsed.debug, config.debug);
        assert_eq!(parsed.server, config.server);
        assert_eq!(parsed.metrics, config.metrics);
        // The one field that is not verbatim, and why: the file provider opens
        // this path against the process CWD, so a draft run from elsewhere
        // would read a different file — or none.
        assert!(parsed.flags.file.path.is_absolute(), "{draft}");
        assert!(parsed.flags.file.path.ends_with("flags.local.yaml"));
        assert_eq!(
            parsed.flags.file.refresh_interval_ms,
            config.flags.file.refresh_interval_ms
        );
    }

    /// True if any value in the document (recursively) is YAML `null`. Walking
    /// the parsed structure — rather than scanning the rendered text for the
    /// substring `null` — means a future host name, contract path, or comment
    /// that happens to contain that substring cannot fail this check for a
    /// reason unrelated to `skip_serializing_if`: only an actual serialized
    /// null *value* counts.
    fn contains_null(value: &serde_yaml::Value) -> bool {
        match value {
            serde_yaml::Value::Null => true,
            serde_yaml::Value::Sequence(seq) => seq.iter().any(contains_null),
            serde_yaml::Value::Mapping(map) => {
                map.values().any(contains_null) || map.keys().any(contains_null)
            }
            serde_yaml::Value::Tagged(tagged) => contains_null(&tagged.value),
            _ => false,
        }
    }

    #[test]
    fn the_draft_renders_no_nulls() {
        // `skip_serializing_if` is not cosmetics policy: a draft is a document
        // an operator reads and edits, and a field added to the model without
        // the attribute turns it into a wall of `null`s. This fails on the next
        // such field rather than at review time.
        let config = hostile_config();
        let draft = draft_of(
            &config,
            &candidate_profile("pat-validate"),
            &DraftOptions::default(),
        );
        let parsed: serde_yaml::Value = serde_yaml::from_str(&draft).expect("valid yaml");
        assert!(!contains_null(&parsed), "{draft}");
    }

    /// A templated route survives the draft verbatim: the match block is
    /// carried forward, so the emitted document must name the template and must
    /// not have grown a `path_prefix:` key (null or otherwise) on its way
    /// through the model.
    #[test]
    fn a_templated_route_round_trips_through_the_draft() {
        let config = matched_config("path_template", "/conversations/{id}");
        let draft = draft_of(
            &config,
            &candidate_profile("conversation"),
            &DraftOptions {
                new_upstream: Some("http://new.internal".to_string()),
                adopt: true,
                ..DraftOptions::default()
            },
        );
        let parsed = assert_valid(&draft);
        assert_eq!(
            parsed.routes[0].r#match.path_template.as_deref(),
            Some("/conversations/{id}")
        );
        assert_eq!(parsed.routes[0].r#match.path_prefix, None);
        // Not merely absent from the parse — absent from the text an operator
        // reads. Comment lines are excluded: the draft's prose mentions the
        // field by name.
        let emitted: String = draft
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            emitted.contains("path_template: /conversations/{id}"),
            "{draft}"
        );
        assert!(!emitted.contains("path_prefix"), "{draft}");
        assert!(!emitted.contains("null"), "{draft}");
    }

    #[test]
    fn a_route_with_no_new_upstream_is_drafted_legacy_only() {
        let config = config_from(
            r#"
observe: {}
routes:
  - id: only-legacy
    match: { methods: ["GET"], path_prefix: "/api" }
    legacy_upstream: "http://legacy.internal"
    mode: legacy_only
"#,
        );
        let draft = draft_of(
            &config,
            &candidate_profile("only-legacy"),
            &DraftOptions::default(),
        );
        let parsed = assert_valid(&draft);
        assert_eq!(parsed.routes[0].mode, RouteMode::LegacyOnly);
        assert_eq!(parsed.routes[0].new_upstream, None);
        assert!(draft.contains("--new-upstream"), "{draft}");

        // …and the flag supplies one.
        let draft = draft_of(
            &config,
            &candidate_profile("only-legacy"),
            &DraftOptions {
                new_upstream: Some("http://new.internal".to_string()),
                adopt: true,
                ..DraftOptions::default()
            },
        );
        let parsed = assert_valid(&draft);
        assert_eq!(parsed.routes[0].mode, RouteMode::ShadowLegacyPrimary);
        assert_eq!(
            parsed.routes[0].new_upstream.as_deref(),
            Some("http://new.internal")
        );
        assert!(parsed.routes[0].comparison.enabled);
    }

    #[test]
    fn a_route_already_serving_from_new_keeps_its_mode() {
        // Re-pointing this at legacy would move live traffic. The draft says so
        // and leaves it alone.
        let config = config_from(
            r#"
observe: {}
routes:
  - id: migrated
    match: { methods: ["GET"], path_prefix: "/api" }
    new_upstream: "http://new.internal"
    mode: new_only
"#,
        );
        let draft = draft_of(
            &config,
            &candidate_profile("migrated"),
            &DraftOptions {
                adopt: true,
                new_upstream: Some("http://other.internal".to_string()),
                ..DraftOptions::default()
            },
        );
        let parsed = assert_valid(&draft);
        assert_eq!(parsed.routes[0].mode, RouteMode::NewOnly);
        assert_eq!(
            parsed.routes[0].new_upstream.as_deref(),
            Some("http://new.internal")
        );
        assert!(!parsed.routes[0].comparison.enabled);
    }

    #[test]
    fn every_configured_route_survives_into_the_draft() {
        let config = config_from(
            r#"
observe: {}
routes:
  - id: a
    match: { methods: ["GET"], path_prefix: "/a" }
    legacy_upstream: "http://legacy.internal"
    mode: legacy_only
  - id: b
    match: { methods: ["GET"], path_prefix: "/b" }
    legacy_upstream: "http://legacy.internal"
    mode: legacy_only
  - id: c
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "http://legacy.internal"
    mode: legacy_only
"#,
        );
        let draft = draft_of(&config, &candidate_profile("a"), &DraftOptions::default());
        let parsed = assert_valid(&draft);
        let ids: Vec<&str> = parsed.routes.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn a_sampled_profile_classifies_nothing() {
        let mut config = hostile_config();
        config.observe = Some(ObserveConfig {
            sample_rate: 0.5,
            ..ObserveConfig::default()
        });
        let mut profile = candidate_profile("pat-validate");
        profile.sample_rate = 0.5;
        let outcome = evaluate(&config, &profile, &opts());
        assert_eq!(outcome.suggestions[0].reason, Reason::PartialSample);
        assert_eq!(outcome.suggestions[0].disposition, Disposition::RelayOnly);
        // A sampled profile is unprofiled in every way that matters here: R0
        // already refused to classify any route, so a draft resting on it is
        // exactly the "nothing was profiled" case exit 20 exists for.
        // Automation must not read this as a successful classification.
        assert_eq!(outcome.exit_code, EXIT_NOTHING_PROFILED);
    }

    #[test]
    fn an_unprofiled_config_exits_twenty() {
        let config = hostile_config();
        let outcome = evaluate(&config, &empty_profile(), &opts());
        assert_eq!(outcome.exit_code, EXIT_NOTHING_PROFILED);
        assert_eq!(outcome.suggestions[0].reason, Reason::NoObservations);
    }

    #[test]
    fn a_config_with_reads_below_the_floor_exits_twenty() {
        let config = hostile_config();
        let mut profile = candidate_profile("pat-validate");
        let route = profile.routes.get_mut("pat-validate").expect("route");
        route.observations = 2;
        route.reads = 2;
        let outcome = evaluate(&config, &profile, &opts());
        assert_eq!(outcome.suggestions[0].reason, Reason::InsufficientReads);
        assert_eq!(outcome.exit_code, EXIT_NOTHING_PROFILED);
    }

    #[test]
    fn a_real_reason_for_every_route_still_exits_zero_with_a_warning() {
        let config = config_from(
            r#"
observe: {}
routes:
  - id: catchall
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "http://legacy.internal"
    mode: legacy_only
"#,
        );
        let outcome = evaluate(&config, &candidate_profile("catchall"), &opts());
        assert_eq!(outcome.suggestions[0].reason, Reason::CatchAll);
        assert_eq!(outcome.exit_code, EXIT_OK);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.contains("no route reached compare_candidate")),
            "{:?}",
            outcome.warnings
        );
    }

    #[test]
    fn a_profile_from_another_config_is_called_out() {
        let config = hostile_config();
        let outcome = evaluate(&config, &candidate_profile("some-other-route"), &opts());
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.contains("some-other-route")),
            "{:?}",
            outcome.warnings
        );
        // The route the config *does* define was classified against nothing.
        assert_eq!(outcome.suggestions[0].reason, Reason::NoObservations);
    }

    #[test]
    fn the_json_surface_is_one_object_per_route() {
        let config = hostile_config();
        let suggestions = classify_all(
            &config,
            &candidate_profile("pat-validate"),
            &thresholds_of(&candidate_profile("pat-validate")),
        );
        let json: serde_json::Value =
            serde_json::from_str(&render_json(&suggestions).expect("json")).expect("parse");
        let entry = &json.as_array().expect("array")[0];
        assert_eq!(entry["route_id"], "pat-validate");
        assert_eq!(entry["disposition"], "compare_candidate");
        assert_eq!(entry["reason"], "stable-repeated-reads");
        assert_eq!(entry["evidence"]["reads"], 34);
    }

    #[test]
    fn narrowing_matches_the_reason_hid_are_named_in_the_comment() {
        let config = hostile_config();
        let mut profile = candidate_profile("pat-validate");
        let route = profile.routes.get_mut("pat-validate").expect("route");
        route.length_varied = 3;
        route.content_types = ["application/json".to_string(), "text/html".to_string()]
            .into_iter()
            .collect();
        let draft = draft_of(&config, &profile, &DraftOptions::default());
        assert!(draft.contains("SUGGESTED: compare_narrowed"), "{draft}");
        assert!(draft.contains("body-varies"), "{draft}");
        assert!(draft.contains("content-type-varies"), "{draft}");
    }

    #[test]
    fn an_all_error_route_names_its_status_mix_in_the_headline() {
        // R8a's claim is about the status map, so the map is quoted where the
        // claim is made: a reader must not have to reconcile "no read ever
        // succeeded" against an evidence line further down to see which
        // failures the route served.
        let config = hostile_config();
        let mut profile = candidate_profile("pat-validate");
        let route = profile.routes.get_mut("pat-validate").expect("route");
        route.status_classes = BTreeMap::from([("4xx".to_string(), 30), ("5xx".to_string(), 4)]);
        // No successes, therefore no stability evidence — the shape a
        // success-qualified recorder emits, and the only one this command's own
        // door would let through to be rendered.
        route.length_repeats = 0;
        assert!(check_profile_consistency(&profile).is_ok());
        let draft = draft_of(&config, &profile, &DraftOptions::default());
        assert!(
            draft.contains("SUGGESTED: relay_only (no-success-evidence)"),
            "{draft}"
        );
        assert!(draft.contains("no read ever succeeded"), "{draft}");
        assert!(draft.contains("4xx×30 5xx×4"), "{draft}");
    }

    #[test]
    fn transport_errors_are_reported_read_scoped_when_the_two_counts_differ() {
        // The number R8a's carve-out actually consulted, beside the one a
        // reader would otherwise assume it consulted.
        let config = hostile_config();
        let mut profile = candidate_profile("pat-validate");
        let route = profile.routes.get_mut("pat-validate").expect("route");
        route.writes = 20;
        route.observations = 54;
        route.transport_errors = 20;
        route.read_transport_errors = 0;
        let draft = draft_of(&config, &profile, &DraftOptions::default());
        assert!(
            draft.contains("20 transport errors (0 on reads)"),
            "{draft}"
        );
    }

    #[test]
    fn the_confirm_caveat_rides_on_narrowed_as_well_as_candidate() {
        let config = hostile_config();
        let mut profile = candidate_profile("pat-validate");
        profile
            .routes
            .get_mut("pat-validate")
            .expect("route")
            .length_varied = 1;
        let draft = draft_of(&config, &profile, &DraftOptions::default());
        assert!(draft.contains("SUGGESTED: compare_narrowed"), "{draft}");
        assert!(
            draft.contains("Observation cannot prove this route does not mutate"),
            "{draft}"
        );
    }

    #[test]
    fn a_relay_only_route_carries_no_confirm_caveat() {
        let config = hostile_config();
        let draft = draft_of(&config, &empty_profile(), &DraftOptions::default());
        assert!(draft.contains("SUGGESTED: relay_only"), "{draft}");
        assert!(
            !draft.contains("Observation cannot prove"),
            "nothing is shadowed on a relay-only route, so the caveat is noise:\n{draft}"
        );
    }

    #[test]
    fn the_profiles_sample_rate_wins_over_the_configs() {
        // The whole point of Fix H: a hand-edited config claiming full coverage
        // cannot talk a sampled profile past R0, because the rate the
        // classifier sees is the one the recorder wrote.
        // The config claims full coverage (its default 1.0); the profile says
        // it was sampled. The classifier must believe the profile — this test
        // discriminates only because the two values differ.
        let config = hostile_config();
        assert_eq!(config.observe.expect("observe").sample_rate, 1.0);
        let mut profile = candidate_profile("pat-validate");
        profile.sample_rate = 0.25;
        let outcome = evaluate(&hostile_config(), &profile, &opts());
        assert_eq!(outcome.suggestions[0].reason, Reason::PartialSample);
    }

    #[test]
    fn a_config_that_disagrees_with_the_profile_is_input_unavailable() {
        let config = hostile_config(); // declares sample_rate 1.0
        let mut profile = candidate_profile("pat-validate");
        profile.sample_rate = 0.25;
        let err = check_config_describes_the_profiled_proxy(&config, &profile).unwrap_err();
        assert_eq!(err.exit_code(), EXIT_INPUT_UNAVAILABLE);
        assert!(err.to_string().contains("0.25"), "{err}");
    }

    #[test]
    fn a_config_without_an_observe_block_is_input_unavailable() {
        let err = check_config_describes_the_profiled_proxy(&Config::default(), &empty_profile())
            .unwrap_err();
        assert_eq!(err.exit_code(), EXIT_INPUT_UNAVAILABLE);
        assert!(err.to_string().contains("observe"), "{err}");
    }

    #[test]
    fn an_agreeing_config_passes_the_cross_check() {
        assert!(
            check_config_describes_the_profiled_proxy(&hostile_config(), &empty_profile()).is_ok()
        );
    }

    /// A profile whose one route carries exactly the counters given, on top of
    /// the candidate-shaped baseline. The consistency check reads four fields,
    /// so the fixtures state all four rather than inheriting any of them.
    fn profile_with(
        reads: u64,
        status: &[(&str, u64)],
        read_transport_errors: u64,
        stability: (u64, u64, u64),
    ) -> ObserveProfile {
        let mut profile = candidate_profile("pat-validate");
        let route = profile.routes.get_mut("pat-validate").expect("route");
        route.observations = reads;
        route.reads = reads;
        route.status_classes = status
            .iter()
            .map(|(class, n)| ((*class).to_string(), *n))
            .collect();
        route.read_transport_errors = read_transport_errors;
        route.length_repeats = stability.0;
        route.length_varied = stability.1;
        route.length_missing = stability.2;
        profile
    }

    #[test]
    fn stability_counters_that_no_success_could_have_produced_are_refused() {
        // Codex's shape: every read answered 5xx and every read was withheld by
        // transport, yet the document claims eleven stable repeats. Under a
        // success-qualified recorder those eleven cannot exist — and left to
        // classify, the shape used the carve-out to slip past R8a and reached
        // compare_candidate on repeats of a body no upstream ever sent. The
        // arithmetic is the tell, so the door reads the arithmetic.
        let profile = profile_with(12, &[("5xx", 12)], 12, (11, 0, 0));
        let err = check_profile_consistency(&profile).expect_err("corrupt counters must refuse");
        assert_eq!(err.exit_code(), EXIT_INPUT_UNAVAILABLE);
        let message = err.to_string();
        // The operator has to be able to act on this: which route, which
        // numbers, and which arithmetic they violated.
        assert!(message.contains("pat-validate"), "{message}");
        assert!(message.contains("11 repeats"), "{message}");
        assert!(message.contains("0 successful reads"), "{message}");
        assert!(message.contains("2xx"), "{message}");
        // Zero successes cannot account for a single repeat, so the shape stays
        // refused under the corrected arithmetic rather than only under the
        // over-strict sum it was first written against.
        assert!(
            check_profile_consistency(&profile_with(12, &[("5xx", 12)], 12, (1, 0, 0))).is_err()
        );
        // And it refuses through the front door too, not merely when called
        // directly — this is the check that has to run before classification.
        assert!(check_config_describes_the_profiled_proxy(&hostile_config(), &profile).is_err());
    }

    #[test]
    fn the_disjoint_stability_counters_are_weighed_together() {
        // Repeats and length-less reads are disjoint — a successful read is one
        // or the other, never both — so each consumes a success and they are
        // weighed as a pair. Individually under the success count, together
        // over it.
        let profile = profile_with(12, &[("2xx", 4), ("4xx", 8)], 0, (3, 0, 2));
        let err = check_profile_consistency(&profile).expect_err("the pair must be weighed");
        let message = err.to_string();
        assert!(message.contains("3 repeats"), "{message}");
        assert!(message.contains("2 reads without a length"), "{message}");
        assert!(message.contains("4 successful reads"), "{message}");
    }

    #[test]
    fn a_length_that_varied_more_often_than_it_repeated_is_refused() {
        // The other half of the corrected arithmetic. A length can only be seen
        // to move on a repeat, and the recorder counts that repeat too, so
        // `varied > repeats` describes an increment that has no read behind it.
        let profile = profile_with(12, &[("2xx", 12)], 0, (2, 3, 0));
        let err = check_profile_consistency(&profile).expect_err("varied cannot outrun repeats");
        assert_eq!(err.exit_code(), EXIT_INPUT_UNAVAILABLE);
        let message = err.to_string();
        assert!(message.contains("pat-validate"), "{message}");
        assert!(message.contains("3 varied"), "{message}");
        assert!(message.contains("2 repeats"), "{message}");
    }

    #[test]
    fn more_read_transport_errors_than_reads_is_refused() {
        // The read-scoped counter is a subset of the reads by construction, so
        // a document where it exceeds them is one the recorder cannot have
        // written — and the excess points the wrong way: it would arm R8a's
        // carve-out on a route whose reads were answered.
        let profile = profile_with(6, &[("4xx", 6)], 7, (0, 0, 0));
        let err = check_profile_consistency(&profile).expect_err("an impossible subset refuses");
        assert_eq!(err.exit_code(), EXIT_INPUT_UNAVAILABLE);
        let message = err.to_string();
        assert!(message.contains("pat-validate"), "{message}");
        assert!(message.contains("read_transport_errors (7)"), "{message}");
        assert!(message.contains("reads (6)"), "{message}");
    }

    #[test]
    fn a_profile_the_recorder_could_have_written_passes() {
        // The control the refusals need. Three shapes a real run produces: a
        // clean candidate, a route whose reads all failed at the upstream (the
        // R8a shape, whose stability counters are consequently zero), and one
        // whose reads were withheld by transport.
        for (name, profile) in [
            (
                "a stable candidate",
                profile_with(12, &[("2xx", 12)], 0, (11, 0, 0)),
            ),
            (
                "errors only, and therefore no stability evidence",
                profile_with(12, &[("4xx", 12)], 0, (0, 0, 0)),
            ),
            (
                "every read withheld by transport",
                profile_with(12, &[("5xx", 12)], 12, (0, 0, 0)),
            ),
        ] {
            assert!(
                check_profile_consistency(&profile).is_ok(),
                "{name} must classify"
            );
        }
    }

    #[test]
    fn stability_exactly_accounted_for_by_the_successes_passes() {
        // The boundary is inclusive: twelve successful reads of one fingerprint
        // are one first sighting and eleven repeats, plus a twelfth read to
        // reach the ceiling. Refusing at equality would refuse the densest
        // legitimate profile there is.
        let exact = profile_with(12, &[("2xx", 12)], 0, (12, 0, 0));
        assert!(check_profile_consistency(&exact).is_ok());
        // Every repeat also varied — a route whose body changes on every call,
        // which is R9's whole subject. `varied` rides *on* those same repeats
        // rather than consuming reads of its own, so this is the shape a real
        // recorder emits and it must not be read as 22 reads' worth of
        // evidence. (The over-strict sum this check was first written with
        // refused exactly this, and slauth's observe-golden run against a live
        // `pat-list` route hit it: 3 successes behind 2 repeats + 2 varied.)
        let every_repeat_varied = profile_with(12, &[("2xx", 12)], 0, (11, 11, 0));
        assert!(check_profile_consistency(&every_repeat_varied).is_ok());
        let split = profile_with(12, &[("2xx", 8), ("4xx", 4)], 4, (5, 5, 3));
        assert!(check_profile_consistency(&split).is_ok());
        // One past it is not.
        let over = profile_with(12, &[("2xx", 12)], 0, (12, 0, 1));
        assert!(check_profile_consistency(&over).is_err());
    }

    #[test]
    fn the_field_shape_that_caught_the_arithmetic_passes() {
        // The regression, verbatim from slauth's observe-golden run against its
        // real `pat-list` route: four reads, three of them successful, two
        // repeats and both of them varied. The recorder produced this document;
        // a check that refused it was wrong about the recorder, not the other
        // way round.
        let profile = profile_with(4, &[("2xx", 3), ("4xx", 1)], 0, (2, 2, 0));
        assert!(
            check_profile_consistency(&profile).is_ok(),
            "the door must admit a document the recorder actually wrote"
        );
        // And it classifies rather than merely parsing. Four reads is below the
        // default floor, so the golden route's own answer is R3 — the door's
        // job here was to let the rules speak at all.
        let outcome = evaluate(&hostile_config(), &profile, &opts());
        assert_eq!(outcome.suggestions[0].reason, Reason::InsufficientReads);
        // The same arithmetic above the floor lands on R9, which is what a
        // route whose body moves on every repeat should be told.
        let above_the_floor = profile_with(13, &[("2xx", 12), ("4xx", 1)], 0, (2, 2, 0));
        let outcome = evaluate(&hostile_config(), &above_the_floor, &opts());
        assert_eq!(outcome.suggestions[0].reason, Reason::BodyVaries);
    }

    /// A one-route config whose route matches `path` written as `field`.
    fn matched_config(field: &str, path: &str) -> Config {
        config_from(&format!(
            r#"
observe: {{}}
routes:
  - id: conversation
    match: {{ methods: ["GET"], {field}: "{path}" }}
    legacy_upstream: "http://legacy.internal"
    mode: legacy_only
"#
        ))
    }

    /// `candidate_profile`, recorded under a named matcher — the provenance the
    /// stale-profile check reads.
    fn candidate_profile_matched(route_id: &str, basis: &str) -> ObserveProfile {
        let mut profile = candidate_profile(route_id);
        profile.routes.get_mut(route_id).expect("route").match_basis = basis.to_string();
        profile
    }

    /// The three ways a route's matcher can move under a profile that has
    /// already been recorded. Each is refused, because the profile's path
    /// counts were taken against a matcher that is no longer in force and the
    /// classifier would read them as if it were.
    #[test]
    fn a_profile_recorded_under_a_different_matcher_is_input_unavailable() {
        for (field, path, recorded) in [
            // A route that was templated after the profile was taken: the
            // recorded per-id path spread would read as one tidy endpoint.
            (
                "path_template",
                "/conversations/{id}",
                "prefix:/conversations/",
            ),
            // …and the reverse: a count of one shape read as a count of one
            // path, which is the same mistake pointing the other way.
            (
                "path_prefix",
                "/conversations/",
                "template:/conversations/{id}",
            ),
            // The template itself moved. `{id}` and `{id}/messages` are
            // different operations, and their profiles are not interchangeable.
            (
                "path_template",
                "/conversations/{id}",
                "template:/conversations/{id}/messages",
            ),
        ] {
            let config = matched_config(field, path);
            let profile = candidate_profile_matched("conversation", recorded);
            let err = check_config_describes_the_profiled_proxy(&config, &profile)
                .expect_err("a profile recorded under another matcher must be refused");
            assert_eq!(err.exit_code(), EXIT_INPUT_UNAVAILABLE);
            let message = err.to_string();
            // Named in full: the operator has to be able to tell which of the
            // two ran ahead of the other.
            assert!(message.contains("conversation"), "{message}");
            assert!(message.contains(recorded), "{message}");
            assert!(message.contains(path), "{message}");
        }
    }

    #[test]
    fn a_profile_recorded_under_the_configured_matcher_passes() {
        // The control the three refusals above need: the check is not simply
        // refusing every templated route.
        for (field, path, recorded) in [
            (
                "path_template",
                "/conversations/{id}",
                "template:/conversations/{id}",
            ),
            ("path_prefix", "/conversations/", "prefix:/conversations/"),
        ] {
            let config = matched_config(field, path);
            let profile = candidate_profile_matched("conversation", recorded);
            assert!(
                check_config_describes_the_profiled_proxy(&config, &profile).is_ok(),
                "{recorded} must classify against {field} {path}"
            );
        }
    }

    #[test]
    fn a_route_the_profile_never_carried_is_not_a_basis_mismatch() {
        // It is classified against a zero-filled profile and lands on
        // `no-observations`. Refusing the whole run because a route saw no
        // traffic would make the command unusable on any config with a quiet
        // route in it.
        let config = matched_config("path_template", "/conversations/{id}");
        assert!(check_config_describes_the_profiled_proxy(&config, &empty_profile()).is_ok());
    }

    #[test]
    fn a_profile_sharing_no_route_id_with_the_config_is_refused() {
        // codex review (C2): with a fully disjoint id set every per-route
        // basis comparison is skipped, and zero comparisons must not read as
        // agreement — this is some other proxy's profile. Partial overlap
        // stays legal (see the quiet-route test above).
        let config = matched_config("path_template", "/conversations/{id}");
        let profile = candidate_profile_matched("some-other-service", "prefix:/api/");
        let err = check_config_describes_the_profiled_proxy(&config, &profile)
            .expect_err("disjoint ids must refuse");
        let msg = err.to_string();
        assert!(msg.contains("share no id"), "{msg}");
        assert!(msg.contains("some-other-service"), "{msg}");
    }

    #[test]
    fn a_templated_routes_path_count_is_labelled_as_normalized() {
        // "1 path" on a route that served thousands of conversations is true
        // and, unlabelled, deeply misleading — the template folded them, which
        // is also why the path-spread rules stayed quiet.
        let config = matched_config("path_template", "/conversations/{id}");
        let profile = candidate_profile_matched("conversation", "template:/conversations/{id}");
        let draft = draft_of(&config, &profile, &DraftOptions::default());
        assert!(draft.contains("1 path (template-normalized)"), "{draft}");

        // A prefix route's count is not relabelled: there it really is a count
        // of distinct paths.
        let config = matched_config("path_prefix", "/conversations/");
        let profile = candidate_profile_matched("conversation", "prefix:/conversations/");
        let draft = draft_of(&config, &profile, &DraftOptions::default());
        assert!(draft.contains("1 path"), "{draft}");
        assert!(!draft.contains("template-normalized"), "{draft}");
    }

    #[test]
    fn a_partially_written_profile_does_not_parse_as_a_pristine_route() {
        // Structurally valid JSON, every danger signal simply absent. Under a
        // container-level `#[serde(default)]` this would parse as a clean,
        // stable, cookie-free, redirect-free route and could reach candidacy.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.json");
        std::fs::write(
            &path,
            r#"{"sample_rate":1.0,"routes":{"r":{"observations":34,"reads":34,"length_repeats":12}}}"#,
        )
        .expect("write");
        let err = load_profile_file(&path).unwrap_err();
        assert_eq!(err.exit_code(), EXIT_INPUT_UNAVAILABLE);
    }

    #[test]
    fn a_profile_without_a_sample_rate_does_not_parse() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.json");
        std::fs::write(&path, r#"{"routes":{}}"#).expect("write");
        let err = load_profile_file(&path).unwrap_err();
        assert_eq!(err.exit_code(), EXIT_INPUT_UNAVAILABLE);
    }

    #[test]
    fn exit_codes_are_this_commands_own_vocabulary() {
        assert_eq!(
            SuggestError::InputUnavailable(String::new()).exit_code(),
            50
        );
        assert_eq!(SuggestError::NeverQuiesced(String::new()).exit_code(), 40);
    }

    #[test]
    fn an_unreadable_profile_file_is_input_unavailable() {
        let err = load_profile_file(Path::new("./definitely-not-a-profile.json")).unwrap_err();
        assert_eq!(err.exit_code(), EXIT_INPUT_UNAVAILABLE);
    }

    #[test]
    fn a_profile_that_is_not_a_profile_is_input_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.json");
        std::fs::write(&path, "{\"routes\": {\"r\": {\"nope\": 1}}}").expect("write");
        let err = load_profile_file(&path).unwrap_err();
        assert_eq!(err.exit_code(), EXIT_INPUT_UNAVAILABLE);
    }

    #[test]
    fn the_profile_round_trips_through_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.json");
        let profile = candidate_profile("pat-validate");
        std::fs::write(&path, serde_json::to_string(&profile).expect("json")).expect("write");
        assert_eq!(load_profile_file(&path).expect("load"), profile);
    }

    #[test]
    fn comment_lines_wrap_at_the_documented_width() {
        let items: Vec<String> = (0..12).map(|i| format!("item-number-{i}")).collect();
        for line in wrap("  evidence: ", "            ", &items) {
            assert!(line.chars().count() <= COMMENT_WIDTH + 20, "{line}");
        }
        assert!(wrap("  evidence: ", "  ", &items).len() > 1);
    }
}
