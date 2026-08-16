//! Route classification: turn one route's observed traffic profile into a
//! *suggested* comparison disposition, with the evidence that produced it.
//!
//! **The governing epistemic limit, and the reason everything below is shaped
//! the way it is: response metadata can prove a route UNSAFE to compare; it can
//! never prove one SAFE.** No observable signal distinguishes `GET /orders/42`
//! from `GET /orders/42/mark-read` when the latter returns a stable 200 JSON
//! body. So this module never asserts a route is safe. It gathers evidence,
//! demotes everything showing a danger signal, and hands a human the residue to
//! confirm against the service's source. The affirmative outcome is
//! [`Disposition::CompareCandidate`] — a hypothesis carrying evidence, not a
//! verdict.
//!
//! Two consequences are load-bearing on the rule table:
//!
//! - **[`Disposition::CompareNarrowed`] is not a safe landing spot for a
//!   mutation suspect.** It still enables comparison, so it still dispatches a
//!   shadow request. Every mutation-suspect signal therefore terminates at
//!   [`Disposition::RelayOnly`]; narrowing is reserved for signals meaning
//!   "comparing this needs a narrower definition of equal", never "this might
//!   mutate".
//! - **Candidacy requires affirmative evidence, not merely the absence of
//!   danger.** The fall-through is reachable only when a request fingerprint
//!   actually repeated with a stable `Content-Length` — which R9/R10/R11
//!   guarantee structurally. "We learned nothing" lands on `compare_narrowed`,
//!   never on candidacy.
//!
//! Rules are evaluated **in order, first match wins**, and every relay-only
//! rule precedes every narrowing rule, so the safe direction always wins a tie
//! (see [`classify`] for the table).
//!
//! What this module is **not**: it performs no I/O, loads no config, reaches no
//! control plane, and renders no draft. It is a pure function of (route config,
//! [`RouteProfile`], [`SuggestThresholds`]) so the rules can be falsified
//! exhaustively in-process — which is the whole point, since a classifier that
//! demotes everything and a classifier that reasons are indistinguishable
//! without a negative control per rule.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Serialize, Serializer};

use crate::config::model::RouteConfig;
use crate::observability::observe::{RouteProfile, OVERSIZED};

/// Default read floor below which a route is not classified at all (R3).
///
/// Five is a floor on *evidence*, not a performance knob: a route seen twice
/// has not demonstrated stability, it has demonstrated nothing. A profiling run
/// against a functional test suite rather than real traffic will trip this on
/// nearly every route — which is the correct answer for that corpus, and why
/// the floor is a caller-supplied threshold rather than a constant.
pub const DEFAULT_MIN_SAMPLES: u64 = 5;

/// Default ceiling on distinct read paths before a route is treated as a
/// wildcard proxy rather than an endpoint (R7).
pub const DEFAULT_MAX_COMPARE_PATHS: u64 = 8;

/// R8's threshold: reads-to-distinct-paths ratio at or above which a route is
/// treated as carrying opaque ids or one-time tokens *in the path*.
///
/// Expressed as a numerator/denominator pair rather than `0.8` because the rule
/// is evaluated in integer arithmetic — see [`opaque_path_ids`].
pub const OPAQUE_PATH_RATIO_NUM: u128 = 4;
/// Denominator of [`OPAQUE_PATH_RATIO_NUM`] (i.e. the ratio is 4/5 = 0.8).
pub const OPAQUE_PATH_RATIO_DEN: u128 = 5;

/// The path prefix that makes a route the unclassified remainder (R1).
const CATCH_ALL_PREFIX: &str = "/";

/// Query-parameter names that mean "this request carries a one-time credential,
/// a flow binding, or a signature" (R6).
///
/// A public, documented vocabulary: consumers assert against these names, and
/// **widening the set is a safe change while narrowing it is not** — a name
/// added here can only demote a route, and every demotion costs evidence rather
/// than safety.
///
/// Entries are lowercase because matching lowercases first (see
/// [`normalized_query_name`]); a mixed-case name like `SAMLRequest` appears here
/// in its folded form.
///
/// Several of these — `state`, `nonce`, `session_state` — ride *every* OAuth
/// authorize request, so including them demotes a large and common family of
/// routes. That is deliberate and is the same reasoning that refuses a PKCE
/// carve-out: those parameters bind a request to a server-side flow record, and
/// a GET that consumes a flow binding is the exact shape this rule exists to
/// keep out of a shadow.
pub const ONE_TIME_TOKEN_NAMES: &[&str] = &[
    "code",
    "token",
    "ticket",
    "flow",
    "verifier",
    "challenge",
    // Flow bindings: identify a server-side record the request may consume.
    "state",
    "nonce",
    "session_state",
    "relaystate",
    // Cross-site request tokens: their presence means the request is
    // state-changing often enough that the framework demanded one.
    "csrf",
    "xsrf",
    // Signatures and message authentication: a signed URL is a capability, and
    // capabilities are usually single-use.
    "sig",
    "signature",
    "hmac",
    "mac",
    "digest",
    // Bare credentials.
    "otp",
    "pin",
    "secret",
    "client_secret",
    // Federation payloads: a signed request/response document in the query.
    "request",
    "id_token_hint",
    "samlrequest",
    "samlresponse",
];

/// Suffixes that carry the same meaning as [`ONE_TIME_TOKEN_NAMES`] under a
/// vendor prefix (`login_verifier`, `consent_challenge`, `csrf_token`, …),
/// matched case-insensitively.
///
/// **There is deliberately no PKCE carve-out.** `code_challenge` matches
/// `_challenge` and demotes, even though PKCE's challenge is not a one-time
/// credential in the sense this rule is about. Carving it out would be an
/// *upgrade* — moving a route toward comparison on the strength of a name — and
/// that is the dangerous direction. Over-demotion costs evidence; under-demotion
/// costs a shadowed mutation.
pub const ONE_TIME_TOKEN_SUFFIXES: &[&str] =
    &["_verifier", "_challenge", "_token", "_ticket", "_code"];

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

/// What the tool suggests doing with a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Disposition {
    /// Nothing observable argues against comparing this route, and a
    /// fingerprint repeated with a stable length. **Not a safety claim** — see
    /// the module docs.
    CompareCandidate,
    /// Worth comparing, but not on the body: something about the responses
    /// makes body equality untrustworthy.
    CompareNarrowed,
    /// Do not shadow this route. Either it shows a danger signal, or too little
    /// was observed to say anything at all.
    RelayOnly,
}

impl Disposition {
    /// The stable machine-readable name. `snake_case`, matching limen's config
    /// vocabulary (`legacy_only`, `shadow_legacy_primary`); reasons are
    /// kebab-case because they are diagnostic labels rather than config values.
    pub fn as_str(self) -> &'static str {
        match self {
            Disposition::CompareCandidate => "compare_candidate",
            Disposition::CompareNarrowed => "compare_narrowed",
            Disposition::RelayOnly => "relay_only",
        }
    }
}

impl Serialize for Disposition {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Why a route landed where it did — one variant per rule, plus the
/// fall-through.
///
/// The string forms are a **public vocabulary**: downstream harnesses assert
/// `reason == "redirecting-read"` to prove a specific rule bit, so a rename is
/// a breaking change to them, not a refactor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    /// R0 — the profile was built from a sampled subset of traffic, so no
    /// existential rule below can be trusted.
    PartialSample,
    /// R1 — the route's configured `path_prefix` is `/`.
    CatchAll,
    /// R2 — nothing was observed on this route at all.
    NoObservations,
    /// R3 — fewer reads than the caller's `min_samples` floor.
    InsufficientReads,
    /// R4 — at least one read answered 3xx or carried `Location`.
    RedirectingRead,
    /// R5 — at least one read set a cookie.
    MintsState,
    /// R6a — the recorded query-parameter names are known to be incomplete, so
    /// R6's existential check could not have seen everything.
    QueryNamesUnrecorded,
    /// R6 — a query parameter named like a one-time credential, either observed
    /// on a read or required by the route's own match conditions.
    OneTimeTokenQuery,
    /// R7 — reads spread over more distinct paths than `max_compare_paths`, or
    /// the recorder's path set overflowed.
    WildcardGranularity,
    /// R8 — nearly every read hit a distinct path.
    OpaquePathIds,
    /// R9 — repeated identical requests returned different lengths.
    BodyVaries,
    /// R10 — no request fingerprint ever repeated, or the fingerprint map
    /// overflowed.
    NoRepeatEvidence,
    /// R11 — at least one read's response lacked `Content-Length`, so the
    /// stability evidence has a hole in it. (The string form stays
    /// `stability-unobserved`: it is a published vocabulary consumers assert
    /// against, and the rule still means "stability was not established".)
    StabilityUnobserved,
    /// R12 — reads returned more than one content-type essence.
    ContentTypeVaries,
    /// The fall-through: a fingerprint repeated with a stable length and no
    /// rule fired. The only reason that accompanies
    /// [`Disposition::CompareCandidate`].
    StableRepeatedReads,
}

impl Reason {
    /// The stable machine-readable name (kebab-case, per the rule table).
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::PartialSample => "partial-sample",
            Reason::CatchAll => "catch-all",
            Reason::NoObservations => "no-observations",
            Reason::InsufficientReads => "insufficient-reads",
            Reason::RedirectingRead => "redirecting-read",
            Reason::MintsState => "mints-state",
            Reason::QueryNamesUnrecorded => "query-names-unrecorded",
            Reason::OneTimeTokenQuery => "one-time-token-query",
            Reason::WildcardGranularity => "wildcard-granularity",
            Reason::OpaquePathIds => "opaque-path-ids",
            Reason::BodyVaries => "body-varies",
            Reason::NoRepeatEvidence => "no-repeat-evidence",
            Reason::StabilityUnobserved => "stability-unobserved",
            Reason::ContentTypeVaries => "content-type-varies",
            Reason::StableRepeatedReads => "stable-repeated-reads",
        }
    }
}

impl Serialize for Reason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// The caller-supplied inputs that are not the profile itself. The two
/// thresholds are floors on *evidence*, so lowering them is a deliberate act a
/// run should record rather than a default worth changing; `sample_rate`
/// describes the profile rather than tuning the rules.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SuggestThresholds {
    /// R3: reads below this and the route is not classified.
    pub min_samples: u64,
    /// R7: distinct read paths above this and the route is a wildcard proxy.
    pub max_compare_paths: u64,
    /// The `observe.sample_rate` the profile was recorded under. Anything below
    /// `1.0` refuses classification outright (R0) — see [`classify`].
    ///
    /// Not `Option`: "unknown" and "sampled" would want the same answer, and a
    /// caller that cannot state the rate has not established that the profile
    /// is complete.
    pub sample_rate: f64,
}

impl Default for SuggestThresholds {
    fn default() -> Self {
        Self {
            min_samples: DEFAULT_MIN_SAMPLES,
            max_compare_paths: DEFAULT_MAX_COMPARE_PATHS,
            sample_rate: 1.0,
        }
    }
}

/// One route's classification.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Suggestion {
    /// The configured route id this describes.
    pub route_id: String,
    pub disposition: Disposition,
    /// The **first** rule that matched. See [`Evidence::narrowing_matches`] for
    /// the ones first-match-wins hid.
    pub reason: Reason,
    pub evidence: Evidence,
}

/// Everything the decision rested on, in enough detail to explain it in a
/// generated comment without re-deriving anything.
///
/// Deliberately not just a copy of [`RouteProfile`]: it carries the *derived*
/// signals (the matched token names, the path-uniqueness ratio, every matched
/// narrowing rule) that are the actual content of a rule firing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Evidence {
    pub observations: u64,
    pub reads: u64,
    pub writes: u64,
    pub transport_errors: u64,
    pub distinct_read_paths: u64,
    pub distinct_read_paths_overflow: bool,
    pub status_classes: BTreeMap<String, u64>,
    pub content_types: BTreeSet<String>,
    pub content_types_overflow: bool,
    pub set_cookie_reads: u64,
    pub redirect_reads: u64,
    pub location_reads: u64,
    pub length_repeats: u64,
    pub length_varied: u64,
    pub length_missing: u64,
    pub fingerprint_overflow: bool,
    /// The **observed** read query-parameter names that matched the
    /// one-time-token vocabulary. Names only; the recorder never sees a value.
    pub one_time_token_names_observed: BTreeSet<String>,
    /// The route's own `match.query_present` names that matched the same
    /// vocabulary — R6's config-derived half.
    ///
    /// Kept in a separate set from the observed one so a consumer can tell the
    /// two sources apart: "traffic carried a verifier" and "this route is
    /// *defined* as the verifier hop" are different facts about a route, and
    /// the second is true even of a route no traffic ever reached.
    pub one_time_token_names_configured: BTreeSet<String>,
    /// The recorded query-name set is known to be incomplete: the recorder's
    /// set overflowed, or a name was too long to record and collapsed to the
    /// oversized sentinel. R6a's signal.
    pub query_names_unrecorded: bool,
    /// `distinct_read_paths / reads`, or `None` when no read was observed.
    /// Reported for the human; R8 itself is evaluated in integer arithmetic.
    pub path_uniqueness_ratio: Option<f64>,
    /// **Every** narrowing rule that matched, in evaluation order — not just
    /// the one that became `reason`.
    ///
    /// First-match-wins is right for the decision and wrong for the
    /// explanation: a route demoted for `body-varies` that *also* serves three
    /// content types needs both facts on the page, and a relay-only route can
    /// carry narrowing matches that its relay reason hid entirely.
    pub narrowing_matches: Vec<Reason>,
}

// ---------------------------------------------------------------------------
// The classifier
// ---------------------------------------------------------------------------

/// Classify one route from its config and its observed profile.
///
/// | # | Reason | Signal | → |
/// |---|---|---|---|
/// | R0 | `partial-sample` | `sample_rate` is not exactly `1.0` | relay-only |
/// | R1 | `catch-all` | `match.path_prefix == "/"` (config, not traffic) | relay-only |
/// | R2 | `no-observations` | `observations == 0` | relay-only |
/// | R3 | `insufficient-reads` | `reads < min_samples` | relay-only |
/// | R4 | `redirecting-read` | `redirect_reads > 0` or `location_reads > 0` | relay-only |
/// | R5 | `mints-state` | `set_cookie_reads > 0` | relay-only |
/// | R6a | `query-names-unrecorded` | query-name overflow, or an oversized name | relay-only |
/// | R6 | `one-time-token-query` | a one-time-token name, observed on a read **or** required by `match.query_present` | relay-only |
/// | R7 | `wildcard-granularity` | `distinct_read_paths > max_compare_paths`, or path overflow | relay-only |
/// | R8 | `opaque-path-ids` | `distinct_read_paths / reads >= 0.8` | relay-only |
/// | R9 | `body-varies` | `length_varied > 0` | narrowed |
/// | R11 | `stability-unobserved` | **any** read lacked `Content-Length` | narrowed |
/// | R10 | `no-repeat-evidence` | `length_repeats == 0`, or fingerprint overflow | narrowed |
/// | R12 | `content-type-varies` | more than one content-type essence, or overflow | narrowed |
/// | — | `stable-repeated-reads` | a fingerprint repeated with a stable length | candidate |
///
/// The rows are ordered as evaluated. Four things about that order are
/// structural rather than incidental:
///
/// - **Every relay-only rule precedes every narrowing rule**, so a profile that
///   matches both lands on relay-only. That is expressed in the code by
///   [`relay_rule`] being consulted before [`narrowing_rules`], not by the
///   sequence of `if`s — the ordering cannot be broken by inserting a rule in
///   the wrong place.
/// - **R0 precedes everything, and makes sampling and classification mutually
///   exclusive.** R4, R5, R6 and R6a are all *existential* (`> 0`): one
///   cookie-minting or redirecting read among ten thousand is enough to condemn
///   a route, and dropping observations wholesale is exactly what sampling
///   does. Under `sample_rate: 0.1` the rare mutating hop is the observation
///   most likely to be missed while the route still clears `min_samples` and
///   reaches candidacy — the profile is not a smaller version of the truth, it
///   is a version with the decisive evidence possibly removed. So a sampled
///   profile is not classified at all: observe cheaply, or classify, not both.
/// - **R6a precedes R6**, so "the query names on record are incomplete"
///   outranks "here is what the query names on record say". A route whose name
///   set overflowed, or that carried a name too long to record (the recorder
///   collapses those to a sentinel rather than truncating a possible
///   credential), is one whose token check could not have seen everything —
///   and R6 is existential, so not seeing everything is not a smaller answer
///   but a possibly wrong one.
/// - **R11 is evaluated before R10**, which is a correction to the rule table
///   rather than a transcription slip. Under the table's original "*every* read
///   lacked `Content-Length`" reading, R11 implied `length_repeats == 0` (a
///   repeat is only counted when both sightings carried a length), so ordered
///   after R10 it could never be reached and would be a rule that existed only
///   in the documentation. R11 now reads "*any* read" (see [`narrowing_rules`]
///   for the HEAD-authorizes-GET bypass that forced it), which makes it strictly
///   broader than the reachability argument needed — but the order stays, since
///   "a read never declared its length" is the more actionable of two labels
///   that share a disposition anyway.
///
/// **The two tiers answer different questions, and merging them would be a
/// safety regression.** Every *mutation-suspect* signal (R4, R5, R6, R6a)
/// terminates at relay-only, because the narrowing tier still enables
/// comparison — and therefore still dispatches a shadow request — under the
/// downstream adopt flag. The narrowing tier is about *body trustworthiness*
/// ("comparing this needs a narrower definition of equal"), never about whether
/// the route mutates. A future reader tempted to collapse the tiers, or to move
/// a mutation signal into the narrowing tier because "it still gets compared
/// either way", would be turning a relay into a shadowed mutation.
///
/// **Standing residuals, named rather than papered over.**
///
/// - **Sub-path aliasing is not fixable from traffic.** Classification is per
///   *route*; mutation is per *path*. A route matching `/orders/` aggregates
///   `GET /orders/42` and `GET /orders/42/mark-read` into one profile, and if
///   the mutating hop is a minority of traffic sharing the majority's shape,
///   R7 (a path count under the ceiling) and R8 (a low uniqueness ratio) both
///   stay quiet and the route reaches candidacy. The recorder deliberately
///   keeps path *hashes* and never a path, so no rule here can see which
///   sub-paths those reads hit; recording them would put user-identifying
///   strings on the control plane, which the profile refuses to do. This is the
///   strongest single argument for the emitted draft never enabling comparison
///   on its own: candidacy has to survive a human reading the route's source.
/// - **`transport_errors` deliberately does not demote.** A silent upstream
///   contributes nothing to the content-type, cookie, redirect or stability
///   evidence (the recorder attributes by origin), so a flapping upstream can
///   only *withhold* evidence, never manufacture it — and withheld evidence is
///   already caught by R3 and R10. Demoting on it would demote healthy routes
///   for their upstream's bad week without making any unsafe route safe.
pub fn classify(
    route: &RouteConfig,
    profile: &RouteProfile,
    thresholds: &SuggestThresholds,
) -> Suggestion {
    let one_time_token_names_observed = one_time_token_names(profile.query_names.iter());
    let one_time_token_names_configured = one_time_token_names(route.r#match.query_present.iter());
    let narrowing_matches = narrowing_rules(profile);

    let evidence = Evidence {
        observations: profile.observations,
        reads: profile.reads,
        writes: profile.writes,
        transport_errors: profile.transport_errors,
        distinct_read_paths: profile.distinct_read_paths,
        distinct_read_paths_overflow: profile.distinct_read_paths_overflow,
        status_classes: profile.status_classes.clone(),
        content_types: profile.content_types.clone(),
        content_types_overflow: profile.content_types_overflow,
        set_cookie_reads: profile.set_cookie_reads,
        redirect_reads: profile.redirect_reads,
        location_reads: profile.location_reads,
        length_repeats: profile.length_repeats,
        length_varied: profile.length_varied,
        length_missing: profile.length_missing,
        fingerprint_overflow: profile.fingerprint_overflow,
        path_uniqueness_ratio: (profile.reads > 0)
            .then(|| profile.distinct_read_paths as f64 / profile.reads as f64),
        one_time_token_names_observed,
        one_time_token_names_configured,
        // Two ways to learn the same thing — the recorder ran out of room, or a
        // name was long enough that recording it verbatim would have put a
        // credential in the profile. Both mean the recorded set is a floor.
        query_names_unrecorded: profile.query_names_overflow
            || profile.query_names.contains(OVERSIZED),
        narrowing_matches,
    };

    // The safe direction is consulted first as a matter of structure: no
    // narrowing rule can pre-empt a relay-only one however the individual rules
    // are later reordered among themselves.
    let (disposition, reason) = match relay_rule(route, profile, &evidence, thresholds) {
        Some(reason) => (Disposition::RelayOnly, reason),
        None => match evidence.narrowing_matches.first() {
            Some(reason) => (Disposition::CompareNarrowed, *reason),
            // Reachable only when no narrowing rule matched, and ¬R10 means a
            // fingerprint repeated while ¬R9 means it repeated at the same
            // length. Candidacy is therefore always backed by an affirmative
            // observation, never by "no rule happened to fire".
            None => (Disposition::CompareCandidate, Reason::StableRepeatedReads),
        },
    };

    Suggestion {
        route_id: route.id.clone(),
        disposition,
        reason,
        evidence,
    }
}

/// R0–R8, in order: the first danger signal, or `None` if the route shows none.
fn relay_rule(
    route: &RouteConfig,
    profile: &RouteProfile,
    evidence: &Evidence,
    thresholds: &SuggestThresholds,
) -> Option<Reason> {
    // R0 — a sampled profile is not a smaller truth, it is a truth with the
    // decisive observation possibly missing. See `classify` for why this
    // outranks even the config-derived rules: there is no route about which a
    // sampled profile says anything sound.
    //
    // Written as "not exactly, verifiably 1.0" rather than `< 1.0`, because
    // `<` is not total over `f64`: every comparison with `NaN` is false, so a
    // `NaN` rate would sail through a `< 1.0` gate and classify a profile whose
    // completeness is not merely partial but unknown. Unknown and incomplete
    // must land in the same place — that is the whole of R0's claim.
    if !thresholds.sample_rate.is_finite() || thresholds.sample_rate != 1.0 {
        return Some(Reason::PartialSample);
    }
    // R1 — read from CONFIG, not traffic. A `/` route *is* the unclassified
    // remainder: comparing it silently shadows every path nobody has looked at,
    // including the writes-in-GET-clothing this whole rule table exists to
    // catch. Traffic cannot tell you this; the route table can.
    if route.r#match.path_prefix.as_deref() == Some(CATCH_ALL_PREFIX) {
        return Some(Reason::CatchAll);
    }
    // R2/R3 — absence is not evidence. An unobserved route is not "clean", and
    // a route seen twice has not demonstrated stability.
    if profile.observations == 0 {
        return Some(Reason::NoObservations);
    }
    if profile.reads < thresholds.min_samples {
        return Some(Reason::InsufficientReads);
    }
    // R4 — unconditional on any 3xx *or* any `Location`, and deliberately
    // independent of parameter names. A redirecting read is the universal shape
    // of an interstitial flow hop, where one-time tokens are consumed and
    // challenges accepted; the canonical writes-in-GET-clothing return a bare
    // 303 with no `Set-Cookie`, so a rule conjoining the two would miss all of
    // them. It also closes the empty-body redirect: plain comparison compares
    // neither `Location` nor `Set-Cookie`, so such a route would be shadowed
    // *and then compare clean*.
    if profile.redirect_reads > 0 || profile.location_reads > 0 {
        return Some(Reason::RedirectingRead);
    }
    // R5 — a read that mints state is a flow-creating read. Comparing one
    // safely needs a human-authored contract narrowing what "equal" means, so
    // this lands on relay-only rather than on narrowing, which would still
    // dispatch the shadow.
    if profile.set_cookie_reads > 0 {
        return Some(Reason::MintsState);
    }
    // R6a — before R6, because R6 is existential and an incomplete set of names
    // cannot answer an existential question. Sits with the other overflow
    // demotions (R7, R10, R12) rather than being the one flag that is read as
    // "nothing there".
    if evidence.query_names_unrecorded {
        return Some(Reason::QueryNamesUnrecorded);
    }
    // R6 — a *supplement* to R4/R5, never the thing standing between a mutating
    // read and a shadow: the traffic half can be defeated by renaming a
    // parameter, which is why it is not load-bearing. Its config half is
    // sturdier and independent of traffic: a route whose `match.query_present`
    // requires a verifier IS the verifier hop by definition, whether or not any
    // read was ever observed on it — the same config-derived reasoning as R1.
    if !evidence.one_time_token_names_observed.is_empty()
        || !evidence.one_time_token_names_configured.is_empty()
    {
        return Some(Reason::OneTimeTokenQuery);
    }
    // R7 — R1 generalized below `/`. Overflow counts: past the recorder's cap
    // the count is a floor, and treating a floor as a measurement is how a
    // safety rule silently stops firing.
    if profile.distinct_read_paths > thresholds.max_compare_paths
        || profile.distinct_read_paths_overflow
    {
        return Some(Reason::WildcardGranularity);
    }
    // R8 — the shape R7's absolute threshold misses: a route where nearly every
    // read hits a distinct path is carrying opaque ids or one-time tokens in
    // the PATH, which R6 cannot see and the recorder deliberately refuses to
    // record.
    if opaque_path_ids(profile) {
        return Some(Reason::OpaquePathIds);
    }
    None
}

/// R9–R12: **every** narrowing rule that matches, in evaluation order.
///
/// All four say the same thing about the body — that equality on it cannot be
/// trusted — which is why they share a disposition and why their relative order
/// only decides which label a human reads.
fn narrowing_rules(profile: &RouteProfile) -> Vec<Reason> {
    let mut matched = Vec::new();
    // R9 — the direct observation: identical requests, different lengths.
    if profile.length_varied > 0 {
        matched.push(Reason::BodyVaries);
    }
    // R11 — ANY read that never declared its length, not every read. The
    // stability map is method-blind: `length_repeats` counts repeats of a
    // fingerprint, and a fingerprint includes the method, so a route whose
    // HEADs repeat at a stable length while its GETs carry no `Content-Length`
    // at all satisfies `length_repeats > 0` (R10 misses) without anything
    // having been learned about the requests that would actually be shadowed.
    // Under the old "every read" reading, R11 missed it too.
    //
    // Candidacy claims *complete* stability evidence, so a single hole in that
    // evidence disqualifies it: "some reads were stable" is a different and
    // weaker claim than the one candidacy makes. Strictly the safe direction,
    // and simpler than a per-method stability map — which would still be
    // method-blind about anything else the fingerprint folds together.
    if profile.length_missing > 0 {
        matched.push(Reason::StabilityUnobserved);
    }
    // R10 — absent evidence read as absent, not as passing. A corpus that hits
    // each endpoint once produces `length_repeats == 0`, which says nothing
    // about stability; and past the fingerprint cap variance goes unrecorded,
    // so overflow is itself a demotion rather than a silent "stable".
    if profile.length_repeats == 0 || profile.fingerprint_overflow {
        matched.push(Reason::NoRepeatEvidence);
    }
    // R12 — a route that answers in more than one media type is not one whose
    // bodies can be compared by a single rule.
    if profile.content_types.len() > 1 || profile.content_types_overflow {
        matched.push(Reason::ContentTypeVaries);
    }
    matched
}

/// R8's predicate: `distinct_read_paths / reads >= 0.8`, in integer arithmetic.
///
/// Integer rather than floating point for two reasons: `reads == 0` is a
/// division by zero rather than a rule (it is R2/R3's territory, but the guard
/// belongs with the arithmetic rather than with the caller that happens to
/// precede it today), and the threshold is a boundary an operator will test
/// exactly — `0.8` is not representable in binary, and a rule that fires at 4/5
/// but not at 400/500 would be indefensible. `u128` because the products are of
/// two `u64`s.
fn opaque_path_ids(profile: &RouteProfile) -> bool {
    if profile.reads == 0 {
        return false;
    }
    u128::from(profile.distinct_read_paths) * OPAQUE_PATH_RATIO_DEN
        >= u128::from(profile.reads) * OPAQUE_PATH_RATIO_NUM
}

/// Those of `query_names` that match the one-time-token vocabulary,
/// case-insensitively. Generic over the source so the observed names and the
/// route's `match.query_present` names are judged by exactly the same test.
fn one_time_token_names<'a>(query_names: impl Iterator<Item = &'a String>) -> BTreeSet<String> {
    query_names
        .filter(|name| is_one_time_token_name(name))
        .cloned()
        .collect()
}

/// Whether one query-parameter name reads as a one-time credential.
fn is_one_time_token_name(name: &str) -> bool {
    let name = normalized_query_name(name);
    ONE_TIME_TOKEN_NAMES.contains(&name.as_str())
        || ONE_TIME_TOKEN_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

/// A query-parameter name folded to the form the vocabulary is written in:
/// percent-decoded, trimmed, lowercased.
///
/// **Decoded here and not in the observer**, deliberately. The profile is a
/// faithful record of what was on the wire — `t%6Fken` is what the client sent,
/// and a recorder that normalized would be editing evidence, and would owe the
/// same normalization to every other consumer of the field. Matching is where
/// the interpretation belongs, so the raw name is what the profile and the
/// [`Evidence`] report while the *comparison* sees through the encoding.
///
/// One decoding pass, not a fixpoint: `%2570` decodes to `%70` rather than `p`.
/// A second pass would defeat that, a third the next layer, and the ladder has
/// no top — the honest boundary is one pass plus R6a, which demotes any route
/// whose recorded names are known to be incomplete.
fn normalized_query_name(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match hex_pair(bytes, index) {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    // Lossy because a crafted `%80` need not be valid UTF-8, and a name that
    // decodes to garbage must still be *compared*, not skipped.
    String::from_utf8_lossy(&decoded)
        .trim()
        .to_ascii_lowercase()
}

/// The byte a `%XX` escape at `index` denotes, if there is one there.
fn hex_pair(bytes: &[u8], index: usize) -> Option<u8> {
    if bytes.get(index)? != &b'%' {
        return None;
    }
    let high = (*bytes.get(index + 1)? as char).to_digit(16)?;
    let low = (*bytes.get(index + 2)? as char).to_digit(16)?;
    u8::try_from(high * 16 + low).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A route with the given prefix. Built by deserializing the config's own
    /// YAML so the test cannot drift from `RouteConfig`'s shape.
    fn route(path_prefix: &str) -> RouteConfig {
        route_matching(path_prefix, &[])
    }

    /// A route whose match is conditioned on the given `query_present` names.
    fn route_matching(path_prefix: &str, query_present: &[&str]) -> RouteConfig {
        let present = query_present
            .iter()
            .map(|name| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(", ");
        serde_yaml::from_str(&format!(
            r#"
id: "test-route"
match:
  methods: ["GET"]
  path_prefix: "{path_prefix}"
  query_present: [{present}]
legacy_upstream: "https://legacy.internal"
mode: legacy_only
"#
        ))
        .expect("route fixture parses")
    }

    fn a_route() -> RouteConfig {
        route("/orders/")
    }

    fn names(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    fn counts(entries: &[(&str, u64)]) -> BTreeMap<String, u64> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), *v))
            .collect()
    }

    /// The baseline: a route whose traffic shows no danger signal and whose
    /// fingerprint repeated at a stable length — i.e. the one shape that
    /// reaches candidacy. Every falsification below is this profile plus one
    /// signal; every negative control is this profile unchanged, so a
    /// classifier that demoted unconditionally would fail every control.
    fn clean_profile() -> RouteProfile {
        RouteProfile {
            observations: 12,
            reads: 12,
            writes: 0,
            transport_errors: 0,
            methods: counts(&[("GET", 12)]),
            query_names: names(&["id"]),
            query_names_overflow: false,
            distinct_read_paths: 1,
            distinct_read_paths_overflow: false,
            status_classes: counts(&[("2xx", 12)]),
            content_types: names(&["application/json"]),
            content_types_overflow: false,
            set_cookie_reads: 0,
            redirect_reads: 0,
            location_reads: 0,
            length_repeats: 11,
            length_varied: 0,
            length_missing: 0,
            fingerprint_overflow: false,
        }
    }

    fn classify_clean(profile: &RouteProfile) -> Suggestion {
        classify(&a_route(), profile, &SuggestThresholds::default())
    }

    #[track_caller]
    fn assert_demoted(profile: &RouteProfile, disposition: Disposition, reason: Reason) {
        let suggestion = classify_clean(profile);
        assert_eq!(
            (suggestion.disposition, suggestion.reason),
            (disposition, reason),
            "expected {} / {}",
            disposition.as_str(),
            reason.as_str()
        );
    }

    #[track_caller]
    fn assert_candidate(profile: &RouteProfile) {
        let suggestion = classify_clean(profile);
        assert_eq!(
            (suggestion.disposition, suggestion.reason),
            (Disposition::CompareCandidate, Reason::StableRepeatedReads),
            "expected a candidate, got {} / {}",
            suggestion.disposition.as_str(),
            suggestion.reason.as_str()
        );
    }

    /// Like [`assert_demoted`], but for the falsifications and controls that
    /// need something other than `a_route()` at the default thresholds — R0's
    /// rate overrides, R1's catch-all prefix, R8's raised ceiling.
    #[track_caller]
    fn assert_classifies(
        route: &RouteConfig,
        profile: &RouteProfile,
        thresholds: &SuggestThresholds,
        disposition: Disposition,
        reason: Reason,
    ) {
        let suggestion = classify(route, profile, thresholds);
        assert_eq!(
            (suggestion.disposition, suggestion.reason),
            (disposition, reason),
            "expected {} / {}, got {} / {}",
            disposition.as_str(),
            reason.as_str(),
            suggestion.disposition.as_str(),
            suggestion.reason.as_str()
        );
    }

    // -- R0 partial sample -------------------------------------------------

    #[test]
    fn r0_falsification_a_sampled_profile_is_not_classified() {
        // Traffic as clean as it gets, but recorded under sampling: the reads
        // that were dropped are the ones this classifier would have condemned
        // the route for, and there is no way to know whether any existed.
        assert_classifies(
            &a_route(),
            &clean_profile(),
            &SuggestThresholds {
                sample_rate: 0.5,
                ..SuggestThresholds::default()
            },
            Disposition::RelayOnly,
            Reason::PartialSample,
        );
    }

    #[test]
    fn r0_falsification_even_a_near_full_sample_is_not_classified() {
        // 0.99 is not 1.0. A single dropped observation is enough to remove the
        // only cookie-minting read a route ever served, and the rules that
        // matter here are existential — so the line is at completeness, not at
        // "enough".
        assert_classifies(
            &a_route(),
            &clean_profile(),
            &SuggestThresholds {
                sample_rate: 0.99,
                ..SuggestThresholds::default()
            },
            Disposition::RelayOnly,
            Reason::PartialSample,
        );
    }

    #[test]
    fn r0_outranks_every_other_rule() {
        // Including the config-derived ones: nothing about a sampled profile is
        // worth reporting a more specific reason for, and a `catch-all` label
        // would imply the traffic was examined.
        let profile = RouteProfile {
            redirect_reads: 9,
            set_cookie_reads: 9,
            length_varied: 9,
            ..clean_profile()
        };
        assert_classifies(
            &route("/"),
            &profile,
            &SuggestThresholds {
                sample_rate: 0.1,
                ..SuggestThresholds::default()
            },
            Disposition::RelayOnly,
            Reason::PartialSample,
        );
    }

    #[test]
    fn r0_falsification_an_unknown_sample_rate_is_not_classified() {
        // `NaN < 1.0` is FALSE — every comparison with NaN is — so a `<` gate
        // would let an unknown completeness through as though it were complete.
        // Unknown and incomplete must land in the same place.
        for rate in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1.5] {
            let suggestion = classify(
                &a_route(),
                &clean_profile(),
                &SuggestThresholds {
                    sample_rate: rate,
                    ..SuggestThresholds::default()
                },
            );
            assert_eq!(
                suggestion.reason,
                Reason::PartialSample,
                "sample_rate {rate} must not classify"
            );
        }
    }

    #[test]
    fn r0_control_a_full_sample_is_a_candidate() {
        assert_eq!(SuggestThresholds::default().sample_rate, 1.0);
        assert_candidate(&clean_profile());
    }

    // -- R1 catch-all -----------------------------------------------------

    #[test]
    fn r1_falsification_catch_all_prefix_demotes() {
        // Traffic as clean as it gets; the route table alone condemns it.
        assert_classifies(
            &route("/"),
            &clean_profile(),
            &SuggestThresholds::default(),
            Disposition::RelayOnly,
            Reason::CatchAll,
        );
    }

    #[test]
    fn r1_control_specific_prefix_is_a_candidate() {
        // `a_route()` already is `route("/orders/")` — a specific prefix — so
        // this is the same shape as R2's control below; the two tests exist
        // for different rules but coincide because there is exactly one way
        // to reach candidacy from `clean_profile()`.
        assert_candidate(&clean_profile());
    }

    // -- R2 no observations -----------------------------------------------

    #[test]
    fn r2_falsification_unobserved_route_demotes() {
        assert_demoted(
            &RouteProfile::default(),
            Disposition::RelayOnly,
            Reason::NoObservations,
        );
    }

    #[test]
    fn r2_control_observed_route_is_a_candidate() {
        assert_candidate(&clean_profile());
    }

    // -- R3 insufficient reads --------------------------------------------

    #[test]
    fn r3_falsification_below_the_floor_demotes() {
        let profile = RouteProfile {
            observations: 4,
            reads: 4,
            length_repeats: 3,
            ..clean_profile()
        };
        assert_demoted(&profile, Disposition::RelayOnly, Reason::InsufficientReads);
    }

    #[test]
    fn r3_control_at_the_floor_is_a_candidate() {
        // Exactly `min_samples`: the boundary is inclusive on the safe side of
        // the comparison (`reads < min_samples` demotes), so 5 passes.
        let profile = RouteProfile {
            observations: 5,
            reads: 5,
            length_repeats: 4,
            ..clean_profile()
        };
        assert_candidate(&profile);
    }

    #[test]
    fn r3_boundary_one_below_the_floor_demotes() {
        let profile = RouteProfile {
            observations: DEFAULT_MIN_SAMPLES - 1,
            reads: DEFAULT_MIN_SAMPLES - 1,
            length_repeats: 3,
            ..clean_profile()
        };
        assert_demoted(&profile, Disposition::RelayOnly, Reason::InsufficientReads);
    }

    #[test]
    fn r3_writes_do_not_count_toward_the_read_floor() {
        // 100 writes and 2 reads is not 102 samples of read behavior.
        let profile = RouteProfile {
            observations: 102,
            reads: 2,
            writes: 100,
            length_repeats: 1,
            ..clean_profile()
        };
        assert_demoted(&profile, Disposition::RelayOnly, Reason::InsufficientReads);
    }

    // -- R4 redirecting read ----------------------------------------------

    #[test]
    fn r4_falsification_bare_redirect_with_innocuous_query_demotes() {
        // The canonical write-in-GET-clothing: a bare 303, NO Set-Cookie, and a
        // parameter named `ref` so R6 cannot be what saves it. If R4 were
        // deleted this profile would reach candidacy.
        let profile = RouteProfile {
            redirect_reads: 12,
            location_reads: 12,
            query_names: names(&["ref"]),
            status_classes: counts(&[("3xx", 12)]),
            ..clean_profile()
        };
        assert_demoted(&profile, Disposition::RelayOnly, Reason::RedirectingRead);
    }

    #[test]
    fn r4_falsification_location_without_a_3xx_status_demotes() {
        // A 200 carrying `Location` is not a status the rule keys on, which is
        // exactly why the rule is a disjunction.
        let profile = RouteProfile {
            location_reads: 1,
            query_names: names(&["ref"]),
            ..clean_profile()
        };
        assert_demoted(&profile, Disposition::RelayOnly, Reason::RedirectingRead);
    }

    #[test]
    fn r4_falsification_redirect_without_a_location_header_demotes() {
        let profile = RouteProfile {
            redirect_reads: 1,
            query_names: names(&["ref"]),
            ..clean_profile()
        };
        assert_demoted(&profile, Disposition::RelayOnly, Reason::RedirectingRead);
    }

    #[test]
    fn r4_control_same_shape_without_the_redirect_is_a_candidate() {
        let profile = RouteProfile {
            redirect_reads: 0,
            location_reads: 0,
            query_names: names(&["ref"]),
            ..clean_profile()
        };
        assert_candidate(&profile);
    }

    // -- R5 mints state ----------------------------------------------------

    #[test]
    fn r5_falsification_cookie_minting_read_with_innocuous_query_demotes() {
        // `id`, not `flow`: R6 must not be what catches this.
        let profile = RouteProfile {
            set_cookie_reads: 1,
            query_names: names(&["id"]),
            ..clean_profile()
        };
        assert_demoted(&profile, Disposition::RelayOnly, Reason::MintsState);
    }

    #[test]
    fn r5_control_same_shape_without_the_cookie_is_a_candidate() {
        let profile = RouteProfile {
            set_cookie_reads: 0,
            query_names: names(&["id"]),
            ..clean_profile()
        };
        assert_candidate(&profile);
    }

    // -- R6 one-time-token query ------------------------------------------

    #[test]
    fn r6_falsification_every_exact_name_demotes() {
        for name in ONE_TIME_TOKEN_NAMES {
            let profile = RouteProfile {
                query_names: names(&[name]),
                ..clean_profile()
            };
            assert_demoted(&profile, Disposition::RelayOnly, Reason::OneTimeTokenQuery);
        }
    }

    #[test]
    fn r6_falsification_every_suffix_demotes() {
        for suffix in ONE_TIME_TOKEN_SUFFIXES {
            let name = format!("login{suffix}");
            let profile = RouteProfile {
                query_names: names(&[&name]),
                ..clean_profile()
            };
            assert_demoted(&profile, Disposition::RelayOnly, Reason::OneTimeTokenQuery);
        }
    }

    #[test]
    fn r6_falsification_matches_case_insensitively() {
        let profile = RouteProfile {
            query_names: names(&["Login_Challenge", "TOKEN"]),
            ..clean_profile()
        };
        assert_demoted(&profile, Disposition::RelayOnly, Reason::OneTimeTokenQuery);
    }

    #[test]
    fn r6_falsification_pkce_challenge_is_not_carved_out() {
        // `code_challenge` is not a one-time credential in PKCE's sense, and it
        // demotes anyway: a carve-out would move a route TOWARD comparison on
        // the strength of a name, which is the dangerous direction.
        let profile = RouteProfile {
            query_names: names(&["client_id", "code_challenge"]),
            ..clean_profile()
        };
        assert_demoted(&profile, Disposition::RelayOnly, Reason::OneTimeTokenQuery);
    }

    #[test]
    fn r6_falsification_percent_encoding_does_not_evade_the_set() {
        // The observer records names raw off the wire — deliberately, so the
        // profile stays a faithful record — which means the classifier owes the
        // decoding. `t%6Fken` is `token`.
        for encoded in ["t%6Fken", "%73tate", "id_token_hi%6Et", "%53AMLRequest"] {
            let profile = RouteProfile {
                query_names: names(&[encoded]),
                ..clean_profile()
            };
            let suggestion = classify_clean(&profile);
            assert_eq!(
                suggestion.reason,
                Reason::OneTimeTokenQuery,
                "{encoded} must not evade the token set"
            );
            // The evidence reports what was on the wire, not the folded form:
            // a human confirming the route needs to see the real parameter.
            assert_eq!(
                suggestion.evidence.one_time_token_names_observed,
                names(&[encoded])
            );
        }
    }

    #[test]
    fn r6_falsification_surrounding_whitespace_does_not_evade_the_set() {
        let profile = RouteProfile {
            query_names: names(&[" nonce "]),
            ..clean_profile()
        };
        assert_demoted(&profile, Disposition::RelayOnly, Reason::OneTimeTokenQuery);
    }

    #[test]
    fn r6_falsification_the_widened_vocabulary_demotes() {
        // Flow bindings, CSRF tokens, signatures, bare credentials and
        // federation payloads. `state` and `nonce` ride every OAuth authorize
        // request, so this demotes a large common family — the accepted
        // direction, on the same logic that refuses a PKCE carve-out.
        for name in [
            "state",
            "nonce",
            "session_state",
            "RelayState",
            "csrf",
            "xsrf",
            "sig",
            "signature",
            "hmac",
            "mac",
            "digest",
            "otp",
            "pin",
            "secret",
            "client_secret",
            "request",
            "id_token_hint",
            "SAMLRequest",
            "SAMLResponse",
        ] {
            let profile = RouteProfile {
                query_names: names(&["id", name]),
                ..clean_profile()
            };
            let suggestion = classify_clean(&profile);
            assert_eq!(
                (suggestion.disposition, suggestion.reason),
                (Disposition::RelayOnly, Reason::OneTimeTokenQuery),
                "{name} must demote"
            );
        }
    }

    #[test]
    fn r6_control_innocuous_names_are_a_candidate() {
        let profile = RouteProfile {
            query_names: names(&["ref", "id", "page", "encoded"]),
            ..clean_profile()
        };
        assert_candidate(&profile);
    }

    #[test]
    fn r6_evidence_names_the_matched_parameters() {
        // `state` matches too — the evidence names every matched parameter, not
        // just whichever one the rule happened to test first.
        let profile = RouteProfile {
            query_names: names(&["id", "login_challenge", "state"]),
            ..clean_profile()
        };
        let suggestion = classify_clean(&profile);
        assert_eq!(
            suggestion.evidence.one_time_token_names_observed,
            names(&["login_challenge", "state"])
        );
        assert!(suggestion
            .evidence
            .one_time_token_names_configured
            .is_empty());
    }

    #[test]
    fn r6_falsification_a_config_declared_verifier_hop_demotes() {
        // A route whose `match.query_present` requires a verifier IS the
        // verifier hop by definition — true of the route table whether or not
        // traffic ever showed the parameter. Traffic here is pristine: stable
        // repeated 200 JSON, innocuous observed query name, no cookie, no
        // redirect. Only the config can condemn it.
        let profile = RouteProfile {
            query_names: names(&["id"]),
            ..clean_profile()
        };
        let suggestion = classify(
            &route_matching("/oauth2/auth", &["login_verifier"]),
            &profile,
            &SuggestThresholds::default(),
        );
        assert_eq!(suggestion.disposition, Disposition::RelayOnly);
        assert_eq!(suggestion.reason, Reason::OneTimeTokenQuery);
        assert_eq!(
            suggestion.evidence.one_time_token_names_configured,
            names(&["login_verifier"])
        );
        // Recorded per source, so a consumer can tell "traffic carried one"
        // from "this route is defined by one".
        assert!(suggestion.evidence.one_time_token_names_observed.is_empty());
    }

    #[test]
    fn r6_control_an_innocuous_query_condition_is_a_candidate() {
        let suggestion = classify(
            &route_matching("/orders/", &["ref"]),
            &clean_profile(),
            &SuggestThresholds::default(),
        );
        assert_eq!(suggestion.disposition, Disposition::CompareCandidate);
        assert!(suggestion
            .evidence
            .one_time_token_names_configured
            .is_empty());
    }

    // -- R6a query names unrecorded ---------------------------------------

    #[test]
    fn r6a_falsification_query_name_overflow_demotes() {
        // The recorder ran out of room, so the recorded names are a floor. R6
        // is existential — "no token name present" is not something an
        // incomplete set can establish.
        let profile = RouteProfile {
            query_names: names(&["id"]),
            query_names_overflow: true,
            ..clean_profile()
        };
        assert_demoted(
            &profile,
            Disposition::RelayOnly,
            Reason::QueryNamesUnrecorded,
        );
    }

    #[test]
    fn r6a_falsification_an_oversized_name_demotes() {
        // The recorder collapses an over-long name to a sentinel rather than
        // truncating it, because a truncated token is still a token prefix. The
        // shape that produces one is a bare query token — `?eyJhbGciOi…` — i.e.
        // precisely a credential in a URL, and it matches no name in the
        // one-time-token vocabulary.
        let profile = RouteProfile {
            query_names: names(&["id", OVERSIZED]),
            ..clean_profile()
        };
        assert_demoted(
            &profile,
            Disposition::RelayOnly,
            Reason::QueryNamesUnrecorded,
        );
    }

    #[test]
    fn r6a_control_a_complete_query_name_set_is_a_candidate() {
        let profile = RouteProfile {
            query_names: names(&["id"]),
            query_names_overflow: false,
            ..clean_profile()
        };
        assert_candidate(&profile);
    }

    #[test]
    fn r6a_outranks_r6() {
        // Both fire; the more honest label wins. "We did not see everything"
        // is a statement about the evidence, "here is a token name" a statement
        // from within it.
        let profile = RouteProfile {
            query_names: names(&["login_challenge"]),
            query_names_overflow: true,
            ..clean_profile()
        };
        assert_demoted(
            &profile,
            Disposition::RelayOnly,
            Reason::QueryNamesUnrecorded,
        );
    }

    // -- R7 wildcard granularity ------------------------------------------

    #[test]
    fn r7_falsification_many_distinct_paths_demote() {
        // Ratio 9/50 = 0.18, well under R8's threshold, so only R7 can fire.
        let profile = RouteProfile {
            observations: 50,
            reads: 50,
            distinct_read_paths: DEFAULT_MAX_COMPARE_PATHS + 1,
            length_repeats: 41,
            ..clean_profile()
        };
        assert_demoted(
            &profile,
            Disposition::RelayOnly,
            Reason::WildcardGranularity,
        );
    }

    #[test]
    fn r7_falsification_path_overflow_demotes() {
        // Past the recorder's cap the count is a floor. Treating a floor as a
        // measurement is how a safety rule silently stops firing.
        let profile = RouteProfile {
            observations: 50,
            reads: 50,
            distinct_read_paths: 2,
            distinct_read_paths_overflow: true,
            length_repeats: 41,
            ..clean_profile()
        };
        assert_demoted(
            &profile,
            Disposition::RelayOnly,
            Reason::WildcardGranularity,
        );
    }

    #[test]
    fn r7_control_at_the_path_ceiling_is_a_candidate() {
        // Exactly `max_compare_paths`: the rule is `>`, so 8 passes. Ratio
        // 8/50 = 0.16 keeps R8 out of it.
        let profile = RouteProfile {
            observations: 50,
            reads: 50,
            distinct_read_paths: DEFAULT_MAX_COMPARE_PATHS,
            length_repeats: 41,
            ..clean_profile()
        };
        assert_candidate(&profile);
    }

    // -- R8 opaque path ids ------------------------------------------------

    #[test]
    fn r8_falsification_nearly_unique_paths_demote() {
        // 4 distinct paths over 5 reads = exactly 0.8, and 4 is under R7's
        // ceiling, so only R8 can fire. This is the path-embedded-token shape
        // that R6 structurally cannot see.
        let profile = RouteProfile {
            observations: 5,
            reads: 5,
            distinct_read_paths: 4,
            length_repeats: 1,
            ..clean_profile()
        };
        assert_demoted(&profile, Disposition::RelayOnly, Reason::OpaquePathIds);
    }

    #[test]
    fn r8_boundary_holds_at_scale() {
        // 400/500 is the same ratio as 4/5 and must classify the same way —
        // the reason the predicate is integer arithmetic rather than an f64
        // comparison against a threshold that binary cannot represent.
        let profile = RouteProfile {
            observations: 500,
            reads: 500,
            distinct_read_paths: 400,
            distinct_read_paths_overflow: false,
            length_repeats: 100,
            ..clean_profile()
        };
        assert_classifies(
            &a_route(),
            &profile,
            &SuggestThresholds {
                // Raised so R7 cannot pre-empt R8 at this scale.
                max_compare_paths: 1000,
                ..SuggestThresholds::default()
            },
            Disposition::RelayOnly,
            Reason::OpaquePathIds,
        );
    }

    #[test]
    fn r8_control_just_under_the_ratio_is_a_candidate() {
        // 7/9 = 0.777…, and 7 is under R7's ceiling.
        let profile = RouteProfile {
            observations: 9,
            reads: 9,
            distinct_read_paths: 7,
            length_repeats: 2,
            ..clean_profile()
        };
        assert_candidate(&profile);
    }

    #[test]
    fn r8_cannot_divide_by_zero() {
        // `reads == 0` is R2/R3 territory, so this is unreachable with the
        // default floor — the guard is asserted anyway, because a rule whose
        // safety depends on another rule running first is one edit away from
        // being wrong. `min_samples: 0` is the only way to reach R8 with no
        // reads at all.
        let profile = RouteProfile {
            observations: 3,
            reads: 0,
            writes: 3,
            methods: counts(&[("POST", 3)]),
            query_names: BTreeSet::new(),
            distinct_read_paths: 0,
            status_classes: counts(&[("2xx", 3)]),
            content_types: BTreeSet::new(),
            length_repeats: 0,
            ..clean_profile()
        };
        let suggestion = classify(
            &a_route(),
            &profile,
            &SuggestThresholds {
                min_samples: 0,
                ..SuggestThresholds::default()
            },
        );
        // No panic, no candidacy: a route with no reads has demonstrated
        // nothing, so it lands on the narrowing side via R10.
        assert_eq!(suggestion.disposition, Disposition::CompareNarrowed);
        assert_eq!(suggestion.reason, Reason::NoRepeatEvidence);
        assert_eq!(suggestion.evidence.path_uniqueness_ratio, None);
    }

    // -- R9 body varies ----------------------------------------------------

    #[test]
    fn r9_falsification_varied_lengths_narrow() {
        let profile = RouteProfile {
            length_varied: 3,
            ..clean_profile()
        };
        assert_demoted(&profile, Disposition::CompareNarrowed, Reason::BodyVaries);
    }

    #[test]
    fn r9_control_stable_lengths_are_a_candidate() {
        let profile = RouteProfile {
            length_varied: 0,
            ..clean_profile()
        };
        assert_candidate(&profile);
    }

    // -- R10 no repeat evidence -------------------------------------------

    #[test]
    fn r10_falsification_no_fingerprint_ever_repeated_narrows() {
        // The shape of a functional test suite: every endpoint hit once. Each
        // response carried a Content-Length (so R11 does not fire), but no
        // request was ever repeated, so nothing was learned about stability.
        let profile = RouteProfile {
            length_repeats: 0,
            length_varied: 0,
            length_missing: 0,
            ..clean_profile()
        };
        assert_demoted(
            &profile,
            Disposition::CompareNarrowed,
            Reason::NoRepeatEvidence,
        );
    }

    #[test]
    fn r10_falsification_fingerprint_overflow_narrows() {
        // Past the cap, variance goes unrecorded — so overflow must itself
        // demote, or the stability signal stops firing exactly when a route is
        // most varied.
        let profile = RouteProfile {
            fingerprint_overflow: true,
            ..clean_profile()
        };
        assert_demoted(
            &profile,
            Disposition::CompareNarrowed,
            Reason::NoRepeatEvidence,
        );
    }

    #[test]
    fn r10_control_one_repeat_is_a_candidate() {
        let profile = RouteProfile {
            length_repeats: 1,
            fingerprint_overflow: false,
            ..clean_profile()
        };
        assert_candidate(&profile);
    }

    // -- R11 stability unobserved -----------------------------------------

    #[test]
    fn r11_falsification_no_read_declared_a_length_narrows() {
        // Chunked responses throughout: the recorder saw every read and could
        // assess none of them.
        let profile = RouteProfile {
            reads: 12,
            length_missing: 12,
            length_repeats: 0,
            length_varied: 0,
            ..clean_profile()
        };
        assert_demoted(
            &profile,
            Disposition::CompareNarrowed,
            Reason::StabilityUnobserved,
        );
    }

    #[test]
    fn r11_control_lengths_present_is_a_candidate() {
        let profile = RouteProfile {
            length_missing: 0,
            ..clean_profile()
        };
        assert_candidate(&profile);
    }

    #[test]
    fn r11_falsification_head_stability_cannot_authorize_get() {
        // THE BYPASS R11 WAS WIDENED FOR. The stability map is method-blind:
        // this route's HEADs repeat at a stable length (so `length_repeats > 0`
        // and R10 misses), while every GET answers without a `Content-Length`
        // at all. Under R11's original "*every* read" reading nothing fired and
        // this reached candidacy — on affirmative evidence about the wrong
        // requests. The GETs are the ones a shadow would replay.
        let profile = RouteProfile {
            observations: 24,
            reads: 24,
            methods: counts(&[("GET", 12), ("HEAD", 12)]),
            length_repeats: 11,
            length_varied: 0,
            length_missing: 12,
            ..clean_profile()
        };
        assert_demoted(
            &profile,
            Disposition::CompareNarrowed,
            Reason::StabilityUnobserved,
        );
    }

    #[test]
    fn r11_falsification_a_single_read_without_a_length_narrows() {
        // One chunked response among twelve is a hole in the evidence.
        // Candidacy claims complete stability, so "eleven of twelve were
        // stable" is not the claim it makes.
        let profile = RouteProfile {
            reads: 12,
            length_missing: 1,
            length_repeats: 10,
            ..clean_profile()
        };
        assert_demoted(
            &profile,
            Disposition::CompareNarrowed,
            Reason::StabilityUnobserved,
        );
    }

    // -- R12 content type varies ------------------------------------------

    #[test]
    fn r12_falsification_two_content_types_narrow() {
        let profile = RouteProfile {
            content_types: names(&["application/json", "text/html"]),
            ..clean_profile()
        };
        assert_demoted(
            &profile,
            Disposition::CompareNarrowed,
            Reason::ContentTypeVaries,
        );
    }

    #[test]
    fn r12_falsification_content_type_overflow_narrows() {
        let profile = RouteProfile {
            content_types: names(&["application/json"]),
            content_types_overflow: true,
            ..clean_profile()
        };
        assert_demoted(
            &profile,
            Disposition::CompareNarrowed,
            Reason::ContentTypeVaries,
        );
    }

    #[test]
    fn r12_control_one_content_type_is_a_candidate() {
        let profile = RouteProfile {
            content_types: names(&["application/json"]),
            content_types_overflow: false,
            ..clean_profile()
        };
        assert_candidate(&profile);
    }

    // -- Ordering ----------------------------------------------------------

    #[test]
    fn relay_only_rules_beat_narrowing_rules() {
        // Matches R5 (relay-only) and R9 + R12 (narrowing) at once. The safe
        // direction must win, and the narrowing matches must survive in the
        // evidence rather than being lost to first-match-wins.
        let profile = RouteProfile {
            set_cookie_reads: 4,
            length_varied: 2,
            content_types: names(&["application/json", "text/html"]),
            ..clean_profile()
        };
        let suggestion = classify_clean(&profile);
        assert_eq!(suggestion.disposition, Disposition::RelayOnly);
        assert_eq!(suggestion.reason, Reason::MintsState);
        assert_eq!(
            suggestion.evidence.narrowing_matches,
            vec![Reason::BodyVaries, Reason::ContentTypeVaries]
        );
    }

    #[test]
    fn earlier_relay_rules_beat_later_ones() {
        // A profile tripping R4 through R8 simultaneously reports the first,
        // which is the most decisive statement about the route.
        let profile = RouteProfile {
            observations: 20,
            reads: 20,
            redirect_reads: 20,
            location_reads: 20,
            set_cookie_reads: 20,
            query_names: names(&["login_challenge"]),
            distinct_read_paths: 19,
            distinct_read_paths_overflow: true,
            length_repeats: 1,
            ..clean_profile()
        };
        assert_demoted(&profile, Disposition::RelayOnly, Reason::RedirectingRead);
    }

    #[test]
    fn evidence_records_every_matched_narrowing_rule() {
        let profile = RouteProfile {
            reads: 12,
            length_varied: 0,
            length_missing: 12,
            length_repeats: 0,
            fingerprint_overflow: true,
            content_types: names(&["application/json", "text/plain"]),
            ..clean_profile()
        };
        let suggestion = classify_clean(&profile);
        assert_eq!(suggestion.reason, Reason::StabilityUnobserved);
        assert_eq!(
            suggestion.evidence.narrowing_matches,
            vec![
                Reason::StabilityUnobserved,
                Reason::NoRepeatEvidence,
                Reason::ContentTypeVaries,
            ]
        );
    }

    // -- The epistemic limit, stated as a test -----------------------------

    #[test]
    fn a_mutating_read_with_no_danger_signal_is_suggested_as_a_candidate() {
        // `GET /orders/42/mark-read`: it mutates on every call, and it answers
        // a stable repeated 200 JSON with no cookie, no redirect, and an
        // innocuous query name. NOTHING in response metadata distinguishes it
        // from `GET /orders/42`.
        //
        // This test asserts the limitation rather than hiding it. The
        // classifier is honest about being unable to see this, which is why
        // `compare_candidate` is a hypothesis for a human to confirm against
        // the service's source and why the emitted draft does not enable
        // comparison on its own.
        let mutating_read = RouteProfile {
            observations: 30,
            reads: 30,
            query_names: names(&["id"]),
            status_classes: counts(&[("2xx", 30)]),
            content_types: names(&["application/json"]),
            length_repeats: 29,
            ..clean_profile()
        };
        assert_candidate(&mutating_read);
    }

    // -- Structural invariants ---------------------------------------------

    #[test]
    fn candidacy_always_rests_on_affirmative_repeat_evidence() {
        // Sweep the flag space rather than trusting the rule order to have been
        // written correctly: whatever combination reaches candidacy must carry
        // a repeated fingerprint at a stable length, and no danger signal.
        // "We learned nothing" must never land here.
        // `length_missing` and the method mix are in the sweep because their
        // absence is what hid the HEAD-authorizes-GET bypass: with only the
        // other flags varied, every profile reaching candidacy happened to have
        // complete length evidence, so the sweep could not have caught a rule
        // that accepted partial evidence.
        let mut checked = 0;
        let mut candidates = 0;
        for bits in 0u32..1 << 10 {
            let head_heavy = (bits >> 9) & 1 == 1;
            let profile = RouteProfile {
                redirect_reads: u64::from(bits & 1),
                set_cookie_reads: u64::from((bits >> 1) & 1),
                location_reads: u64::from((bits >> 2) & 1),
                length_varied: u64::from((bits >> 3) & 1),
                length_repeats: u64::from((bits >> 4) & 1) * 7,
                fingerprint_overflow: (bits >> 5) & 1 == 1,
                distinct_read_paths_overflow: (bits >> 6) & 1 == 1,
                content_types_overflow: (bits >> 7) & 1 == 1,
                length_missing: u64::from((bits >> 8) & 1) * 6,
                methods: if head_heavy {
                    counts(&[("GET", 6), ("HEAD", 6)])
                } else {
                    counts(&[("GET", 12)])
                },
                ..clean_profile()
            };
            let suggestion = classify_clean(&profile);
            checked += 1;
            if suggestion.disposition != Disposition::CompareCandidate {
                continue;
            }
            candidates += 1;
            assert_eq!(suggestion.reason, Reason::StableRepeatedReads);
            assert!(profile.length_repeats > 0, "candidacy without a repeat");
            assert_eq!(profile.length_varied, 0, "candidacy with varied lengths");
            assert!(!profile.fingerprint_overflow, "candidacy past the cap");
            // The bypass, asserted as an invariant rather than as one case: no
            // candidate may carry a read whose length was never seen, whatever
            // the other reads did.
            assert_eq!(
                profile.length_missing, 0,
                "candidacy with a hole in the length evidence"
            );
            assert_eq!(profile.redirect_reads, 0);
            assert_eq!(profile.location_reads, 0);
            assert_eq!(profile.set_cookie_reads, 0);
            assert!(!profile.distinct_read_paths_overflow);
            assert!(!profile.content_types_overflow);
        }
        assert_eq!(checked, 1024);
        // The sweep must not have become vacuous: if nothing reaches candidacy
        // the assertions above are unexecuted and this test proves nothing.
        assert!(candidates > 0, "no shape reached candidacy");
    }

    #[test]
    fn a_default_profile_is_never_a_candidate() {
        // Zero-filled is what every configured route looks like before traffic.
        // Absence must not read as cleanliness.
        for prefix in ["/", "/orders/"] {
            let suggestion = classify(
                &route(prefix),
                &RouteProfile::default(),
                &SuggestThresholds::default(),
            );
            assert_eq!(suggestion.disposition, Disposition::RelayOnly);
        }
    }

    // -- The published vocabulary ------------------------------------------

    #[test]
    fn reason_strings_are_the_documented_vocabulary() {
        // Spelled out rather than derived: these strings are asserted against
        // by downstream harnesses, so a rename must break a test here.
        let expected = [
            (Reason::PartialSample, "partial-sample"),
            (Reason::CatchAll, "catch-all"),
            (Reason::NoObservations, "no-observations"),
            (Reason::InsufficientReads, "insufficient-reads"),
            (Reason::RedirectingRead, "redirecting-read"),
            (Reason::MintsState, "mints-state"),
            (Reason::QueryNamesUnrecorded, "query-names-unrecorded"),
            (Reason::OneTimeTokenQuery, "one-time-token-query"),
            (Reason::WildcardGranularity, "wildcard-granularity"),
            (Reason::OpaquePathIds, "opaque-path-ids"),
            (Reason::BodyVaries, "body-varies"),
            (Reason::NoRepeatEvidence, "no-repeat-evidence"),
            (Reason::StabilityUnobserved, "stability-unobserved"),
            (Reason::ContentTypeVaries, "content-type-varies"),
            (Reason::StableRepeatedReads, "stable-repeated-reads"),
        ];
        for (reason, name) in expected {
            assert_eq!(reason.as_str(), name);
            assert_eq!(
                serde_json::to_string(&reason).expect("reason serializes"),
                format!("\"{name}\"")
            );
        }
    }

    #[test]
    fn disposition_strings_are_the_documented_vocabulary() {
        let expected = [
            (Disposition::CompareCandidate, "compare_candidate"),
            (Disposition::CompareNarrowed, "compare_narrowed"),
            (Disposition::RelayOnly, "relay_only"),
        ];
        for (disposition, name) in expected {
            assert_eq!(disposition.as_str(), name);
            assert_eq!(
                serde_json::to_string(&disposition).expect("disposition serializes"),
                format!("\"{name}\"")
            );
        }
    }

    #[test]
    fn a_suggestion_serializes_with_its_evidence() {
        let suggestion = classify_clean(&clean_profile());
        let json: serde_json::Value =
            serde_json::to_value(&suggestion).expect("suggestion serializes");
        assert_eq!(json["route_id"], "test-route");
        assert_eq!(json["disposition"], "compare_candidate");
        assert_eq!(json["reason"], "stable-repeated-reads");
        assert_eq!(json["evidence"]["reads"], 12);
        assert_eq!(json["evidence"]["length_repeats"], 11);
        assert_eq!(json["evidence"]["path_uniqueness_ratio"], 1.0 / 12.0);
        assert!(json["evidence"]["narrowing_matches"]
            .as_array()
            .expect("narrowing_matches is an array")
            .is_empty());
    }
}
