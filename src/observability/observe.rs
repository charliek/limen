//! Observe mode: passive profiling of the traffic limen already relays.
//!
//! The recorder builds a **bounded per-route aggregate** of response metadata —
//! status, a handful of headers, the request's method and query-parameter
//! *names* — and serves it as one JSON document on the control plane. It is
//! strictly passive (plan 012 §D2): no second upstream contact, and **never a
//! body byte**, because buffering a body to fingerprint it would delay the
//! client's first byte on every request (safety invariant 2). The honest
//! residual cost is one bounded map update under a lock on the response path.
//!
//! Two properties the rest of the feature leans on:
//!
//! - **Absence ≠ zero.** Every configured route is present from construction,
//!   zero-filled, so "the observer never saw this route" and "no such route"
//!   are distinguishable without a metrics round-trip. Traffic never adds a
//!   key — an unknown route id is dropped rather than inserted, so the map's
//!   key set is exactly the config's.
//! - **A count is only as meaningful as the matcher that produced it.** A
//!   templated route (`/conversations/{id}`) folds every id into one shape for
//!   the distinct-read-path count — absorbing that cardinality is what the
//!   template is for, and re-counting the ids underneath it would report the
//!   number the operator wrote the template to stop reporting. So each route
//!   records the matcher it was profiled under ([`RouteProfile::match_basis`]),
//!   and `limen suggest-routes` refuses a profile whose basis its config
//!   contradicts. The *stability* fingerprint is deliberately exempt and stays
//!   on the raw concrete path: two different resources must never register as
//!   one request repeating.
//! - **Only successes vouch.** The stability map admits upstream reads whose
//!   status class is [`prometheus::SUCCESS_STATUS_CLASS`] and nothing else,
//!   because a fixed-length 404 page repeating is not evidence about the body
//!   the operation returns when it works. Every *danger* counter stays
//!   all-reads:
//!   an error response can still condemn a route (it lands in
//!   [`RouteProfile::status_classes`], where the classifier's R8a reads it),
//!   it just cannot vouch for one.
//! - **No wall clock, canonical order.** [`ObserveProfile`] is `BTreeMap`
//!   -ordered throughout and carries no timestamp, uptime, or counter that
//!   advances without traffic, so two scrapes of an idle proxy are
//!   byte-identical. `limen suggest-routes` polls for exactly that identity;
//!   a clock field would make an idle proxy never quiesce.
//!
//! Against safety invariant 5 (never log secret values): the profile is a new
//! output surface, and it emits query-parameter **names** but never a value,
//! never a path, never a header, never a body — so nothing redaction covers can
//! reach it. Paths are counted through a set of hashes, which makes emitting
//! one structurally impossible rather than merely unimplemented. The two
//! remaining strings that come off the wire — a bare query token with no `=`,
//! and an upstream's content-type essence — are length-capped and replaced with
//! [`OVERSIZED`] past the cap, so an entropy-bearing token cannot ride in as a
//! "name".
//!
//! **The seam is [`crate::http::proxy`]'s `handle`, after `dispatch` returns**,
//! and it records the *final client-facing response*. That is the only placement
//! that covers every served response: `dispatch` hands back a client response
//! from eight distinct sites — the primary result arms, the failover-safe
//! replay, and four local refusals (no configured upstream URL, an unforwardable
//! path, an unreadable request body, a failover body too large to buffer) — and
//! a seam inside `dispatch` misses most of them. A hot `failover_to_legacy` +
//! `failover_safe` route reading `observations: 0` forever would be exactly the
//! absence≠zero confusion this module exists to prevent. Profiling what the
//! route actually returns to clients is also the more honest question.
//!
//! The one fact the final response cannot carry is where it came from, so
//! `dispatch` reports it explicitly as a [`ResponseOrigin`]. Without it,
//! `transport_errors` would be a guess, and — worse — limen's own fixed-length
//! 502 body would register as a *stable* response, which is the unsafe
//! direction for a classifier that reads stability as evidence a route is safe
//! to shadow.

use std::collections::hash_map::{Entry, RandomState};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::BuildHasher;
use std::sync::{Mutex, PoisonError};

use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, Method, StatusCode};
use serde::{Deserialize, Serialize};

use crate::config::model::ObserveConfig;
use crate::http::shadow;
use crate::observability::prometheus;
use crate::routing::PathMatcher;

/// Control-plane path the observe profile is served from.
///
/// Lives here rather than beside the handler because
/// [`crate::config::validate`] needs it too: the metrics path is
/// operator-supplied and registered on the same router, and axum panics at
/// router *build* time on a duplicate route. Validating the collision turns
/// that abort into a refuse-to-start (invariant 7).
pub const OBSERVE_PROFILE_PATH: &str = "/observe/profile";

/// Ceiling on each of observe's three per-route caps.
///
/// A minimum alone does not satisfy invariant 6: a bound an operator can set to
/// `999999999` is not a bound, and each of these caps sizes a map whose keys
/// come from live traffic. Four figures is the defensible line — worst-cased,
/// one route holds `1024` query names (each capped at [`MAX_QUERY_NAME_LEN`]),
/// `1024` path hashes and `1024` fingerprint entries, which is a few hundred
/// kilobytes per route rather than an operator-configurable memory amplifier.
/// Above it there is nothing left to learn: a route with more than a thousand
/// distinct query names or path shapes has already answered the question the
/// field exists to answer, and the overflow flag carries the rest.
pub const MAX_OBSERVE_BOUND: usize = 1024;

/// Cap on the distinct response content-type essences kept per route.
///
/// Not one of the operator-tunable bounds: a route serving more than eight
/// media types has already answered the only question this field exists to
/// answer ("does this route speak one content type or several?"), so there is
/// nothing to tune.
const MAX_CONTENT_TYPES: usize = 8;

/// Query pairs scanned per request before the scan stops and flags overflow.
///
/// limen sets no URI length limit, so without this a client could hand the
/// response path `?a1&a2&…&aN` and buy `O(N log N)` sort work and `O(N)`
/// transient allocation per request. The *entry* caps bound what is kept; this
/// bounds what is touched. Independent of `observe.max_query_names` — whichever
/// binds first sets the same `query_names_overflow` flag, which already means
/// exactly "we did not see everything".
const MAX_QUERY_PAIRS_SCANNED: usize = 128;

/// Longest query-parameter name recorded verbatim.
///
/// A query token with no `=` is a name by this module's parse, and bare tokens
/// are how bearer credentials show up in a URL (`?eyJhbGciOi…`). No real
/// parameter name approaches 64 bytes; a JWT clears it comfortably.
const MAX_QUERY_NAME_LEN: usize = 64;

/// Longest content-type essence recorded verbatim. Upstream-controlled and
/// unbounded on the wire, so it is capped like a name — but higher, because
/// registered media types genuinely reach the sixties (the
/// `application/vnd.openxmlformats-officedocument.*` family).
const MAX_CONTENT_TYPE_LEN: usize = 128;

/// Stands in for any string that exceeded its length cap. A fixed sentinel
/// rather than a truncation: a truncated token is still a token prefix.
pub const OVERSIZED: &str = "<oversized>";

/// Methods recorded under their own name. Anything else is folded into
/// [`OTHER_METHOD`] — a route's `match.methods` bounds what can reach it today,
/// but the method map must stay bounded on its own rather than by that.
const KNOWN_METHODS: &[&str] = &[
    "GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "TRACE", "CONNECT",
];

/// The bucket every unknown/extension method lands in.
const OTHER_METHOD: &str = "other";

/// Where the response the client received came from.
///
/// The one fact about a served response that reading the response cannot
/// recover: limen's synthesized 502 and an upstream's own 502 are
/// indistinguishable on the wire. `dispatch` knows which it produced, so it
/// says so rather than letting the recorder infer it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseOrigin {
    /// An upstream answered and limen relayed it, so the response's status and
    /// headers describe what the route actually serves.
    Upstream,
    /// An upstream was contacted and never produced a usable response — a
    /// transport error, a timeout, or a body that failed mid-read — so the
    /// status is limen's own.
    UpstreamSilent,
    /// limen refused before contacting any upstream: no configured upstream
    /// URL, a path that cannot be forwarded byte-for-byte, an unreadable
    /// request body, or a failover body too large to buffer for replay.
    Refused,
}

/// The whole profile: every configured route, zero-filled until observed,
/// carrying the sample rate the recorder actually applied.
///
/// **Nothing here defaults.** The document is machine-produced and
/// machine-consumed, so a missing field is a corrupt or truncated profile, not
/// a field worth guessing: `deny_unknown_fields` alone would accept a partial
/// object and silently zero-fill every danger signal the classifier reads
/// (`set_cookie_reads`, `redirect_reads`, the overflow flags), which is the one
/// direction a safety input must never fail in. Deny-unknown does not
/// deny-missing, so the container defaults are absent by design.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserveProfile {
    /// The `observe.sample_rate` this profile was recorded under.
    ///
    /// **The proxy that did the sampling is the authoritative source**, so the
    /// rate travels with the document rather than being read from whatever
    /// config a later tool is handed. `limen suggest-routes` refuses to
    /// classify a sampled profile (its rule R0), and a rate taken from a
    /// config the operator supplies is a rate a mismatched or hand-edited
    /// config can misstate — the profile cannot corroborate it, so the safety
    /// rule would be bypassable. It carries no clock and does not advance with
    /// traffic, so it costs the byte-identity quiescence contract nothing.
    pub sample_rate: f64,
    /// Per-route aggregates, keyed by route id. `BTreeMap` for canonical
    /// ordering — see the module docs on quiescence.
    pub routes: BTreeMap<String, RouteProfile>,
}

/// The bounded aggregate for one configured route.
///
/// Every "reads only" field below is restricted to `GET`/`HEAD` requests: the
/// classifier's question is whether a route's *reads* are safe to shadow, and a
/// write's response says nothing about that.
///
/// Every field is required on the wire — see [`ObserveProfile`] for why a
/// partially-written profile must fail to parse rather than read as a pristine
/// route. `Default` remains for *Rust-side* construction (the recorder's
/// zero-filled state and tests), which is a different question from what a
/// document is allowed to omit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteProfile {
    /// The matcher this route was profiled under, verbatim: `prefix:/devices`
    /// or `template:/conversations/{id}` (see
    /// [`crate::routing::PathMatcher::basis`]).
    ///
    /// **The sample-rate precedent applied to the matcher.** It travels with
    /// the document because the proxy that recorded it is the only party that
    /// knows it: under a template, `distinct_read_paths` counts *shapes* and
    /// under a prefix it counts *paths*, so the same number means two different
    /// things and the classifier's wildcard/opaque-path rules read it
    /// differently. A tool that re-derived the basis from whatever config it
    /// was handed would silently reinterpret a profile recorded before the
    /// route was templated; `limen suggest-routes` compares the two instead and
    /// refuses the run when they disagree.
    ///
    /// Required on the wire like every field here — a profile from a binary
    /// that predates this field must fail to parse rather than default to a
    /// basis nobody recorded.
    pub match_basis: String,
    /// Responses served on this route (after sampling), successes and failures
    /// alike, so this reconciles against `limen_requests_total`.
    pub observations: u64,
    /// Observations whose request method was a read (`GET`/`HEAD`).
    pub reads: u64,
    /// Observations whose request method was anything else.
    pub writes: u64,
    /// Observations where an upstream was contacted and never answered
    /// ([`ResponseOrigin::UpstreamSilent`]), so limen synthesized the status.
    /// Reported by `dispatch` rather than inferred from the response, which
    /// cannot tell a relayed 502 from a synthesized one.
    ///
    /// **All observations, reads and writes alike** — the one counter here that
    /// is not read-scoped, because a silent upstream is a fact about the
    /// upstream rather than about the route's reads. See
    /// [`Self::read_transport_errors`] for the read-scoped half.
    pub transport_errors: u64,
    /// The read-scoped subset of [`Self::transport_errors`]: reads where the
    /// upstream never answered.
    ///
    /// Recorded separately because the whole-route counter cannot answer the
    /// question a read rule needs to ask. The classifier's R8a carve-out is
    /// "did *every read* fail to reach an upstream" — withheld evidence only
    /// withholds — and a route whose writes are timing out while its reads are
    /// answering 404 has a `transport_errors` larger than its `reads` without a
    /// single read having been withheld. Compared against `reads`, the
    /// unscoped counter would silently disarm the rule on exactly that route.
    pub read_transport_errors: u64,
    /// Request method → count, bounded by [`KNOWN_METHODS`].
    pub methods: BTreeMap<String, u64>,
    /// Distinct query-parameter **names** seen on reads. Names only — a value
    /// is never read, let alone stored — and any name past
    /// [`MAX_QUERY_NAME_LEN`] is recorded as [`OVERSIZED`].
    pub query_names: BTreeSet<String>,
    /// A name arrived past `observe.max_query_names`, or the query carried more
    /// than [`MAX_QUERY_PAIRS_SCANNED`] pairs, so this set is a floor.
    pub query_names_overflow: bool,
    /// How many distinct paths this route's reads hit. A count, never a path.
    ///
    /// Counted over [`crate::routing::PathMatcher::observed_path`], so a
    /// templated route reports the number of distinct *shapes* it matched —
    /// which is always `1`, since a template names exactly one. Read it
    /// together with [`Self::match_basis`], which says which of the two
    /// questions this number answers.
    pub distinct_read_paths: u64,
    /// A new path arrived past `observe.max_path_shapes`, so the count above
    /// is a floor rather than the truth.
    pub distinct_read_paths_overflow: bool,
    /// Read response status *class* (`2xx`…) → count, of what the client was
    /// served.
    pub status_classes: BTreeMap<String, u64>,
    /// Distinct read response content-type essences (parameters stripped),
    /// from upstream responses only.
    pub content_types: BTreeSet<String>,
    /// A ninth content type arrived and was not recorded.
    pub content_types_overflow: bool,
    /// Upstream read responses carrying `Set-Cookie`.
    pub set_cookie_reads: u64,
    /// Upstream read responses with a 3xx status.
    pub redirect_reads: u64,
    /// Upstream read responses carrying a `Location` header.
    pub location_reads: u64,
    /// Successful reads whose request fingerprint had been seen before *and*
    /// whose upstream response carried a `Content-Length` both times.
    ///
    /// **Success-qualified**, like the two fields below: only upstream reads
    /// whose status class is [`prometheus::SUCCESS_STATUS_CLASS`] enter the
    /// stability map at all. An error response says nothing about how the
    /// operation's normal body behaves — a 404 page is a fixed length on every
    /// route that has one, so counting it manufactures exactly the affirmative
    /// evidence candidacy rests on out of a route that has never once
    /// succeeded. Errors stay fully
    /// visible to every *danger* signal ([`Self::status_classes`], the redirect
    /// and cookie counters, the query names, the distinct-path count), which is
    /// the asymmetry the module is built on: an error can condemn a route, it
    /// cannot vouch for one.
    pub length_repeats: u64,
    /// Of those repeats, how many changed length. `> 0` means the route's
    /// successful responses are not stable across identical requests.
    ///
    /// A **subset of [`Self::length_repeats`]**, not a bucket beside it: a
    /// length can only be seen to move on a repeat, and the recorder counts
    /// that repeat as well. `length_varied <= length_repeats` therefore holds
    /// of every profile, and a consumer that adds the two together is
    /// double-counting the same reads.
    pub length_varied: u64,
    /// Successful upstream reads whose response carried no `Content-Length`, so
    /// stability could not be assessed at all.
    pub length_missing: u64,
    /// A new fingerprint arrived past `observe.max_fingerprints` and was
    /// dropped.
    ///
    /// **Load-bearing.** Past the cap, variance goes unrecorded — without this
    /// flag the stability signal would silently stop firing and a route would
    /// look stable because Limen stopped looking, which is the one direction
    /// this signal must never fail in.
    pub fingerprint_overflow: bool,
}

/// One served response's worth of observable metadata, borrowed from the live
/// request and the client-facing response. Constructed at the seam and consumed
/// immediately, so nothing here outlives the response it describes.
pub struct Observation<'a> {
    method: &'a Method,
    path: &'a str,
    query: Option<&'a str>,
    status: StatusCode,
    /// The **client-facing** response headers, read by reference — the recorder
    /// pulls four values out and never clones the map. Only consulted when
    /// `origin` says an upstream produced them.
    headers: &'a HeaderMap,
    origin: ResponseOrigin,
}

impl<'a> Observation<'a> {
    /// Describe one response limen served. `status`/`headers` are the client's,
    /// whatever produced them; `origin` says which of `dispatch`'s outcomes did.
    pub fn new(
        method: &'a Method,
        path: &'a str,
        query: Option<&'a str>,
        status: StatusCode,
        headers: &'a HeaderMap,
        origin: ResponseOrigin,
    ) -> Self {
        Self {
            method,
            path,
            query,
            status,
            headers,
            origin,
        }
    }
}

/// Everything derived from one observation, computed **before** the lock is
/// taken. Parsing, sorting, hashing and lowercasing are all bounded but not
/// free, and none of them needs the shared map; doing them inside the critical
/// section would put client-controlled work on a lock every other in-flight
/// request is waiting for.
struct Derived<'a> {
    method: &'static str,
    is_read: bool,
    origin: ResponseOrigin,
    status_class: &'static str,
    /// Sorted, deduped, scan-bounded — raw, since the length cap applies only
    /// to what is *recorded*, and the fingerprint never leaves the process.
    query_names: Vec<&'a str>,
    query_scan_overflow: bool,
    path_key: u64,
    fingerprint: u64,
    /// `None` unless an upstream produced the response and set the header.
    content_type: Option<String>,
    set_cookie: bool,
    redirect: bool,
    location: bool,
    content_length: Option<u64>,
}

/// The shared, bounded per-route recorder. Cheap to construct, safe to share
/// across every request task.
///
/// One lock over the whole route map: the critical section is a handful of map
/// updates with no I/O, no `await` and no client-sized work, so it can neither
/// block on anything nor be held across a yield point. If contention ever shows
/// up, this type is the isolated place to shard it.
pub struct ObserveRecorder {
    config: ObserveConfig,
    /// Each configured route's compiled path expression, immutable from
    /// construction and therefore readable **outside** the lock — which is
    /// where it has to be read, since normalizing a path is part of the
    /// pre-lock derivation and the lock protects only the counters. Its key set
    /// is `routes`' key set by construction (both are built from one pass over
    /// the same descriptors), so a lookup here answers the same question a
    /// lookup there would.
    matchers: BTreeMap<String, PathMatcher>,
    /// Randomly keyed, built once and reused for every path and fingerprint
    /// hash.
    ///
    /// **The key must not be fixed.** `DefaultHasher::new()` is deterministic
    /// across processes, so colliding paths could be *crafted*: two crafted
    /// paths take the `contains` hit-path, `distinct_read_paths` reads low, and
    /// `distinct_read_paths_overflow` never fires — which is precisely what
    /// makes the classifier's wildcard/opaque-path safety rules NOT fire. That
    /// is the unsafe direction. A fingerprint collision is the *safe* direction
    /// (it merges two shapes and can only manufacture a spurious
    /// `length_varied`, i.e. a false demotion), but both use this hasher: one
    /// keyed hasher is simpler to reason about than a rule about which call
    /// site may be predictable.
    hasher: RandomState,
    routes: Mutex<BTreeMap<String, RouteState>>,
}

impl ObserveRecorder {
    /// Build a recorder over exactly the configured routes, each zero-filled
    /// but already carrying the matcher it will be profiled under.
    ///
    /// Takes `(id, matcher)` descriptors rather than bare ids because the
    /// matcher answers two questions the recorder cannot answer without it:
    /// what [`RouteProfile::match_basis`] should say, and what a read's
    /// distinct-path key is. Taking both from one descriptor is what makes the
    /// recorded basis and the normalization it describes impossible to
    /// desynchronize.
    ///
    /// Zero-registers the observe counter here rather than at startup because
    /// this is the one place that knows both that observation is on *and* the
    /// configured route id set — the same reasoning as
    /// [`prometheus::register_verdict_series`], applied to a per-route series.
    pub fn new<'a>(
        config: ObserveConfig,
        routes: impl IntoIterator<Item = (&'a str, &'a PathMatcher)>,
    ) -> Self {
        let mut matchers = BTreeMap::new();
        let mut states = BTreeMap::new();
        for (id, matcher) in routes {
            states.insert(id.to_string(), RouteState::new(matcher.basis()));
            matchers.insert(id.to_string(), matcher.clone());
        }
        prometheus::register_observe_series(states.keys().map(String::as_str));
        Self {
            config,
            matchers,
            hasher: RandomState::new(),
            routes: Mutex::new(states),
        }
    }

    /// Record one observation against `route_id`, or drop it if the route is
    /// not one of the configured ones (traffic must never grow the map).
    pub fn record(&self, route_id: &str, obs: Observation<'_>) {
        // Same sampling idiom as the shadow path, including its `<= 0` / `>= 1`
        // edges — one definition, so "sampled" means the same thing in both.
        if !shadow::sampled(self.config.sample_rate) {
            return;
        }
        // An unconfigured route id is dropped here rather than after the
        // derivation, which is the same answer the locked map would give.
        let Some(matcher) = self.matchers.get(route_id) else {
            return;
        };
        // Everything client-sized happens here, outside the lock.
        let derived = self.derive(&obs, matcher);
        {
            // Recovering from poisoning rather than propagating it: a panic
            // while holding this lock must not turn every subsequent request
            // into a panic. Observation is never allowed to break the data
            // plane.
            let mut routes = self.routes.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(state) = routes.get_mut(route_id) else {
                return;
            };
            state.merge(&self.config, &derived);
        }
        prometheus::observe_observation(route_id);
    }

    /// Snapshot the whole profile.
    pub fn profile(&self) -> ObserveProfile {
        let routes = self.routes.lock().unwrap_or_else(PoisonError::into_inner);
        ObserveProfile {
            // Reported from the recorder's own config, which *is* the rate that
            // was applied — not from any config a downstream tool is handed.
            sample_rate: self.config.sample_rate,
            routes: routes
                .iter()
                .map(|(id, state)| (id.clone(), state.profile.clone()))
                .collect(),
        }
    }

    fn derive<'a>(&self, obs: &Observation<'a>, matcher: &PathMatcher) -> Derived<'a> {
        let is_read = shadow::method_is_read(obs.method);
        // Only an upstream response says anything about what the route serves;
        // limen's own error pages have a status the client saw (honest) but a
        // body and headers that are limen's, not the route's.
        let from_upstream = obs.origin == ResponseOrigin::Upstream;

        let mut query_scan_overflow = false;
        let query_names = if is_read {
            query_names(obs.query, &mut query_scan_overflow)
        } else {
            Vec::new()
        };

        Derived {
            method: method_label(obs.method),
            is_read,
            origin: obs.origin,
            status_class: prometheus::status_class(obs.status.as_u16()),
            // Normalized: on a templated route every concrete id keys to the
            // template, so the count answers "how many shapes" rather than
            // "how many ids".
            path_key: self.hasher.hash_one(matcher.observed_path(obs.path)),
            // **Raw**, and deliberately not normalized. The fingerprint asks
            // whether the *same request* repeated, and under a template every
            // resource on the route would otherwise share one key — turning
            // `/conversations/a` followed by `/conversations/b` into a repeat,
            // and their two unrelated body sizes into either false stability or
            // a false `length_varied`. Normalizing here would corrupt the one
            // affirmative signal candidacy rests on.
            fingerprint: self.hasher.hash_one((
                obs.method.as_str(),
                obs.path,
                query_names.as_slice(),
            )),
            query_names,
            query_scan_overflow,
            content_type: from_upstream
                .then(|| content_type_essence(obs.headers))
                .flatten(),
            set_cookie: from_upstream && obs.headers.contains_key(SET_COOKIE),
            redirect: from_upstream && obs.status.is_redirection(),
            location: from_upstream && obs.headers.contains_key(LOCATION),
            content_length: from_upstream.then(|| content_length(obs.headers)).flatten(),
        }
    }
}

/// A route's aggregate plus the bounded sets backing the two fields that are
/// *counts* rather than contents.
#[derive(Debug, Default)]
struct RouteState {
    profile: RouteProfile,
    /// Hashes of the distinct read paths seen, normalized by the route's
    /// matcher (see [`crate::routing::PathMatcher::observed_path`]). Hashes,
    /// not paths: the profile promises never to emit a path, and storing only a
    /// hash makes that a property of the type rather than of the serializer.
    read_paths: HashSet<u64>,
    /// Fingerprint → the `Content-Length` last seen for it. Keyed by hash for
    /// the same reason, and because a key built from the raw query names would
    /// be sized by the client.
    fingerprints: HashMap<u64, u64>,
}

impl RouteState {
    /// A zero-filled route that already knows what it was profiled under.
    fn new(match_basis: String) -> Self {
        Self {
            profile: RouteProfile {
                match_basis,
                ..RouteProfile::default()
            },
            ..Self::default()
        }
    }

    /// Merge one pre-derived observation. Everything here is a bounded map
    /// update — see [`Derived`] for why nothing is computed at this point.
    fn merge(&mut self, config: &ObserveConfig, d: &Derived<'_>) {
        let p = &mut self.profile;
        p.observations += 1;
        bump(&mut p.methods, d.method);

        if !d.is_read {
            p.writes += 1;
            if d.origin == ResponseOrigin::UpstreamSilent {
                p.transport_errors += 1;
            }
            return;
        }
        p.reads += 1;

        // Request-side facts, plus the class of what the client was actually
        // served — true of every served response, however it was produced, so
        // the class histogram reconciles with `limen_requests_total` rather
        // than quietly omitting failures.
        if d.query_scan_overflow {
            p.query_names_overflow = true;
        }
        for name in &d.query_names {
            insert_bounded(
                &mut p.query_names,
                &mut p.query_names_overflow,
                config.max_query_names,
                bounded(name, MAX_QUERY_NAME_LEN),
            );
        }
        if !self.read_paths.contains(&d.path_key) {
            if self.read_paths.len() >= config.max_path_shapes {
                p.distinct_read_paths_overflow = true;
            } else {
                self.read_paths.insert(d.path_key);
                p.distinct_read_paths = self.read_paths.len() as u64;
            }
        }
        bump(&mut p.status_classes, d.status_class);

        match d.origin {
            // Nothing below describes a response the route produced. In
            // particular the stability map is left alone: limen's own 502 page
            // is a fixed length, so counting it would manufacture a *stable*
            // route out of a dead upstream — the unsafe direction.
            ResponseOrigin::UpstreamSilent => {
                p.transport_errors += 1;
                p.read_transport_errors += 1;
                return;
            }
            ResponseOrigin::Refused => return,
            ResponseOrigin::Upstream => {}
        }

        if let Some(essence) = &d.content_type {
            insert_bounded(
                &mut p.content_types,
                &mut p.content_types_overflow,
                MAX_CONTENT_TYPES,
                essence,
            );
        }
        if d.set_cookie {
            p.set_cookie_reads += 1;
        }
        if d.redirect {
            p.redirect_reads += 1;
        }
        if d.location {
            p.location_reads += 1;
        }

        // Success-qualified: only a 2xx read says anything about the body the
        // operation normally returns. An error body is a different document
        // produced by a different code path — usually a fixed-length error page
        // — so admitting one manufactures stability out of a route that has
        // never succeeded, which is the same failure mode the `UpstreamSilent`
        // arm above refuses for limen's own 502. The error is not discarded: it
        // is already in `status_classes` above, where R8a reads it and demotes
        // the route outright.
        //
        // `Refused` and `UpstreamSilent` have already returned, so nothing limen
        // synthesized can reach this call however it was statused — the origin
        // check does not depend on synthesized responses happening to be
        // non-2xx.
        if d.status_class == prometheus::SUCCESS_STATUS_CLASS {
            self.record_stability(config, d);
        }
    }

    /// The one stability signal available without touching a body:
    /// `Content-Length` variance across repeats of the same request
    /// fingerprint — `(method, path hash, sorted query-parameter names)`, never
    /// a value, never a header.
    ///
    /// Called only for a **successful upstream read** (see the caller): the map
    /// is the affirmative half of the evidence, and only a response the route
    /// actually produced on its success path belongs in it.
    ///
    /// Its limits are real and documented rather than papered over: two
    /// requests with different credentials share a fingerprint (a false
    /// "varied", the safe direction), and two different bodies of equal length
    /// look stable (a false "stable", which is why stability is necessary for
    /// candidacy but never sufficient).
    fn record_stability(&mut self, config: &ObserveConfig, d: &Derived<'_>) {
        let Some(length) = d.content_length else {
            self.profile.length_missing += 1;
            return;
        };
        let seen = self.fingerprints.len();
        match self.fingerprints.entry(d.fingerprint) {
            Entry::Occupied(mut previous) => {
                self.profile.length_repeats += 1;
                if *previous.get() != length {
                    self.profile.length_varied += 1;
                    previous.insert(length);
                }
            }
            // A first sighting proves nothing either way — only a repeat can.
            Entry::Vacant(slot) if seen < config.max_fingerprints => {
                slot.insert(length);
            }
            Entry::Vacant(_) => self.profile.fingerprint_overflow = true,
        }
    }
}

/// A bounded method label: a known verb, or [`OTHER_METHOD`]. `&'static str`
/// rather than `String` so the common path allocates nothing.
fn method_label(method: &Method) -> &'static str {
    let name = method.as_str();
    KNOWN_METHODS
        .iter()
        .find(|known| **known == name)
        .copied()
        .unwrap_or(OTHER_METHOD)
}

/// Increment a bounded count map, allocating the key only on first sight.
fn bump(map: &mut BTreeMap<String, u64>, key: &str) {
    if let Some(count) = map.get_mut(key) {
        *count += 1;
    } else {
        map.insert(key.to_string(), 1);
    }
}

/// The distinct query-parameter **names** in a query string, sorted, and
/// whether the scan stopped short of the end.
///
/// Splitting on the first `=` and dropping the remainder unread is what keeps a
/// value from ever reaching the profile: the value is never bound to a name
/// here, so no later change to this module can accidentally record one.
///
/// The scan stops at [`MAX_QUERY_PAIRS_SCANNED`] *before* pushing, rather than
/// collecting everything and truncating — the point of the cap is that a long
/// query never buys the work in the first place.
fn query_names<'a>(query: Option<&'a str>, scan_overflow: &mut bool) -> Vec<&'a str> {
    let Some(query) = query else {
        return Vec::new();
    };
    let mut names: Vec<&str> = Vec::new();
    for (scanned, pair) in query.split('&').enumerate() {
        if scanned >= MAX_QUERY_PAIRS_SCANNED {
            *scan_overflow = true;
            break;
        }
        let name = pair.split_once('=').map_or(pair, |(name, _)| name);
        if !name.is_empty() {
            names.push(name);
        }
    }
    names.sort_unstable();
    names.dedup();
    names
}

/// Insert into a bounded set, flagging overflow rather than silently dropping.
/// Takes `&str` so a repeat — the common case, and this runs under the lock —
/// allocates nothing.
fn insert_bounded(set: &mut BTreeSet<String>, overflow: &mut bool, cap: usize, value: &str) {
    if set.contains(value) {
        return;
    }
    if set.len() >= cap {
        *overflow = true;
        return;
    }
    set.insert(value.to_string());
}

/// `value`, or [`OVERSIZED`] when it is long enough that it is carrying
/// something other than what the field is named for.
fn bounded(value: &str, max: usize) -> &str {
    if value.len() > max {
        OVERSIZED
    } else {
        value
    }
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The media-type essence: everything before the first `;`, lowercased.
/// Parameters are stripped because they carry per-response noise (`boundary=`
/// is effectively unbounded) and say nothing about what the route serves. The
/// essence itself is upstream-controlled and unbounded on the wire, so it is
/// length-capped like a query name.
fn content_type_essence(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(CONTENT_TYPE)?.to_str().ok()?;
    let essence = value.split(';').next()?.trim();
    if essence.is_empty() {
        return None;
    }
    Some(bounded(essence, MAX_CONTENT_TYPE_LEN).to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::routing::CompiledTemplate;

    /// The prefix route every test below records against unless it is about
    /// templates.
    fn prefix(path: &str) -> PathMatcher {
        PathMatcher::Prefix(path.to_string())
    }

    fn template(path: &str) -> PathMatcher {
        PathMatcher::Template(CompiledTemplate::parse(path).expect("a template"))
    }

    fn recorder(config: ObserveConfig) -> ObserveRecorder {
        ObserveRecorder::new(config, [("r", &prefix("/"))])
    }

    /// A one-route recorder whose route matches `path` as a template rather
    /// than as a prefix.
    fn templated_recorder(path: &str) -> ObserveRecorder {
        ObserveRecorder::new(ObserveConfig::default(), [("r", &template(path))])
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    /// Record one relayed response for `method`.
    fn observe(
        rec: &ObserveRecorder,
        method: &Method,
        path: &str,
        query: Option<&str>,
        status: u16,
        hdrs: &HeaderMap,
    ) {
        rec.record(
            "r",
            Observation::new(
                method,
                path,
                query,
                StatusCode::from_u16(status).unwrap(),
                hdrs,
                ResponseOrigin::Upstream,
            ),
        );
    }

    /// Record one relayed read and return the route's profile.
    fn observe_read(
        rec: &ObserveRecorder,
        path: &str,
        query: Option<&str>,
        status: u16,
        hdrs: &HeaderMap,
    ) {
        observe(rec, &Method::GET, path, query, status, hdrs);
    }

    /// Record one response limen synthesized itself.
    fn observe_synthesized(
        rec: &ObserveRecorder,
        method: &Method,
        path: &str,
        query: Option<&str>,
        status: u16,
        origin: ResponseOrigin,
    ) {
        // limen's own error responses are plain-text bodies with framing
        // headers, which is exactly why the recorder must not read them.
        let hdrs = headers(&[
            ("content-type", "text/plain; charset=utf-8"),
            ("content-length", "31"),
        ]);
        rec.record(
            "r",
            Observation::new(
                method,
                path,
                query,
                StatusCode::from_u16(status).unwrap(),
                &hdrs,
                origin,
            ),
        );
    }

    fn profile(rec: &ObserveRecorder) -> RouteProfile {
        rec.profile().routes.get("r").unwrap().clone()
    }

    #[test]
    fn every_configured_route_is_present_and_zero_filled() {
        let rec = ObserveRecorder::new(
            ObserveConfig::default(),
            [("a", &prefix("/a")), ("b", &template("/b/{id}"))],
        );
        let profile = rec.profile();
        assert_eq!(profile.routes.len(), 2);
        // Zero-filled, not merely present: a route nobody hit must be
        // distinguishable from one that does not exist. The one thing a
        // zero-filled route does carry is its match basis — a stale profile is
        // detectable even for a route no traffic ever reached.
        let zero = |basis: &str| RouteProfile {
            match_basis: basis.to_string(),
            ..RouteProfile::default()
        };
        assert_eq!(profile.routes["a"], zero("prefix:/a"));
        assert_eq!(profile.routes["b"], zero("template:/b/{id}"));
    }

    #[test]
    fn traffic_never_adds_a_route_key() {
        let rec = recorder(ObserveConfig::default());
        rec.record(
            "not-configured",
            Observation::new(
                &Method::GET,
                "/x",
                None,
                StatusCode::OK,
                &HeaderMap::new(),
                ResponseOrigin::Upstream,
            ),
        );
        let profile = rec.profile();
        assert_eq!(profile.routes.len(), 1);
        assert_eq!(profile.routes["r"].observations, 0);
    }

    #[test]
    fn counts_reads_writes_and_methods() {
        let rec = recorder(ObserveConfig::default());
        let empty = HeaderMap::new();
        observe_read(&rec, "/a", None, 200, &empty);
        observe(&rec, &Method::HEAD, "/a", None, 200, &empty);
        observe(&rec, &Method::POST, "/a", None, 200, &empty);
        let p = profile(&rec);
        assert_eq!(p.observations, 3);
        assert_eq!(p.reads, 2);
        assert_eq!(p.writes, 1);
        assert_eq!(p.methods["GET"], 1);
        assert_eq!(p.methods["HEAD"], 1);
        assert_eq!(p.methods["POST"], 1);
    }

    #[test]
    fn an_extension_method_folds_into_other() {
        let rec = recorder(ObserveConfig::default());
        let method = Method::from_bytes(b"PROPFIND").unwrap();
        observe(&rec, &method, "/a", None, 200, &HeaderMap::new());
        let p = profile(&rec);
        assert_eq!(p.methods[OTHER_METHOD], 1);
        assert!(!p.methods.contains_key("PROPFIND"));
    }

    #[test]
    fn records_query_names_and_never_a_value() {
        let rec = recorder(ObserveConfig::default());
        let empty = HeaderMap::new();
        observe_read(
            &rec,
            "/a",
            Some("token=s3cret&page=p4ge-value&flag"),
            200,
            &empty,
        );
        let p = profile(&rec);
        assert!(p.query_names.contains("token"));
        assert!(p.query_names.contains("page"));
        // A valueless parameter still has a name.
        assert!(p.query_names.contains("flag"));
        let json = serde_json::to_string(&rec.profile()).unwrap();
        for value in ["s3cret", "p4ge-value"] {
            assert!(!json.contains(value), "a value must never appear: {json}");
        }
    }

    #[test]
    fn an_oversized_bare_token_is_recorded_as_a_sentinel() {
        // The invariant-5 residual: a query token with no `=` parses as a
        // *name*, which is how a bearer credential in a URL would otherwise
        // land in the profile verbatim.
        let rec = recorder(ObserveConfig::default());
        let jwt = format!(
            "eyJhbGciOiJIUzI1NiJ9.{}.sig",
            "a".repeat(MAX_QUERY_NAME_LEN)
        );
        observe_read(&rec, "/a", Some(&jwt), 200, &HeaderMap::new());
        let p = profile(&rec);
        assert!(p.query_names.contains(OVERSIZED));
        let json = serde_json::to_string(&rec.profile()).unwrap();
        assert!(
            !json.contains("eyJhbGciOiJIUzI1NiJ9"),
            "a token must never appear, even as a prefix: {json}"
        );
    }

    #[test]
    fn an_oversized_content_type_is_recorded_as_a_sentinel() {
        let rec = recorder(ObserveConfig::default());
        let essence = format!("application/x-{}", "z".repeat(MAX_CONTENT_TYPE_LEN));
        observe_read(
            &rec,
            "/a",
            None,
            200,
            &headers(&[("content-type", &essence)]),
        );
        assert!(profile(&rec).content_types.contains(OVERSIZED));
        // But a long-and-legitimate registered type still records verbatim.
        let rec = recorder(ObserveConfig::default());
        let real = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
        observe_read(&rec, "/a", None, 200, &headers(&[("content-type", real)]));
        assert!(profile(&rec).content_types.contains(real));
    }

    #[test]
    fn a_very_long_query_stops_scanning_and_flags_overflow() {
        // The cap bounds CPU and transient allocation per request, not just the
        // stored entry count — and it announces itself with the flag that
        // already means "we did not see everything".
        let rec = recorder(ObserveConfig::default());
        let query: String = (0..MAX_QUERY_PAIRS_SCANNED * 4)
            .map(|n| format!("p{n}&"))
            .collect();
        observe_read(&rec, "/a", Some(&query), 200, &HeaderMap::new());
        let p = profile(&rec);
        assert!(p.query_names_overflow);
        // The entry cap binds below the scan cap at the default config, so the
        // set is the smaller of the two — never the whole query.
        assert!(p.query_names.len() <= MAX_QUERY_PAIRS_SCANNED);
        assert!(!p
            .query_names
            .contains(&format!("p{}", MAX_QUERY_PAIRS_SCANNED * 4 - 1)));
    }

    #[test]
    fn query_names_are_bounded_and_flag_overflow() {
        let rec = recorder(ObserveConfig {
            max_query_names: 2,
            ..ObserveConfig::default()
        });
        observe_read(&rec, "/a", Some("a=1&b=2&c=3"), 200, &HeaderMap::new());
        let p = profile(&rec);
        assert_eq!(p.query_names.len(), 2);
        assert!(p.query_names_overflow);
    }

    #[test]
    fn writes_contribute_no_read_only_fields() {
        let rec = recorder(ObserveConfig::default());
        let hdrs = headers(&[
            ("content-type", "application/json"),
            ("content-length", "9"),
        ]);
        for _ in 0..2 {
            observe(&rec, &Method::POST, "/a", Some("q=1"), 200, &hdrs);
        }
        let p = profile(&rec);
        assert_eq!(p.writes, 2);
        assert!(p.query_names.is_empty());
        assert_eq!(p.distinct_read_paths, 0);
        assert!(p.status_classes.is_empty());
        assert!(p.content_types.is_empty());
        // Crucially, a repeated *write* must not manufacture read stability:
        // the classifier reads `length_repeats > 0` as affirmative evidence.
        assert_eq!(p.length_repeats, 0);
    }

    #[test]
    fn counts_distinct_read_paths_without_emitting_one() {
        let rec = recorder(ObserveConfig::default());
        let empty = HeaderMap::new();
        for path in ["/a/1", "/a/2", "/a/1"] {
            observe_read(&rec, path, None, 200, &empty);
        }
        let p = profile(&rec);
        assert_eq!(p.distinct_read_paths, 2);
        assert!(!p.distinct_read_paths_overflow);
        let json = serde_json::to_string(&rec.profile()).unwrap();
        assert!(!json.contains("/a/1"), "no path may be emitted: {json}");
    }

    #[test]
    fn path_shapes_are_bounded_and_flag_overflow() {
        let rec = recorder(ObserveConfig {
            max_path_shapes: 2,
            ..ObserveConfig::default()
        });
        let empty = HeaderMap::new();
        for path in ["/a/1", "/a/2", "/a/3", "/a/4"] {
            observe_read(&rec, path, None, 200, &empty);
        }
        let p = profile(&rec);
        assert_eq!(p.distinct_read_paths, 2, "the count is a floor at the cap");
        assert!(p.distinct_read_paths_overflow);
    }

    #[test]
    fn a_templated_route_counts_shapes_rather_than_ids() {
        // The feature in one assertion: a template names one operation, so
        // eight conversations are one distinct read path, not eight. Absorbing
        // that cardinality is the point — the classifier's wildcard and
        // opaque-path rules are then asking about the *operation*.
        let rec = templated_recorder("/conversations/{id}");
        let empty = HeaderMap::new();
        for n in 0..8 {
            observe_read(&rec, &format!("/conversations/c{n}"), None, 200, &empty);
        }
        let p = profile(&rec);
        assert_eq!(p.reads, 8);
        assert_eq!(p.distinct_read_paths, 1);
        assert!(!p.distinct_read_paths_overflow);

        // The control: the same traffic under a prefix route still counts every
        // id, because under a prefix that count is the only evidence the route
        // is a subtree rather than an endpoint.
        let rec = recorder(ObserveConfig::default());
        for n in 0..8 {
            observe_read(&rec, &format!("/conversations/c{n}"), None, 200, &empty);
        }
        assert_eq!(profile(&rec).distinct_read_paths, 8);
    }

    #[test]
    fn a_templated_route_repeats_only_on_the_same_concrete_path() {
        // Normalization stops at the distinct-path key. The fingerprint asks a
        // different question — did *this request* repeat — and normalizing it
        // would make every resource on the route look like a repeat of every
        // other.
        let rec = templated_recorder("/conversations/{id}");
        let hdrs = headers(&[("content-length", "42")]);
        for _ in 0..3 {
            observe_read(&rec, "/conversations/a", None, 200, &hdrs);
        }
        let p = profile(&rec);
        assert_eq!(p.length_repeats, 2, "the same resource, fetched again");
        assert_eq!(p.length_varied, 0);
    }

    #[test]
    fn two_resources_of_equal_length_are_not_a_repeat() {
        // The false-stability direction: two different conversations that
        // happen to be the same size must not read as one stable request.
        let rec = templated_recorder("/conversations/{id}");
        let hdrs = headers(&[("content-length", "42")]);
        observe_read(&rec, "/conversations/a", None, 200, &hdrs);
        observe_read(&rec, "/conversations/b", None, 200, &hdrs);
        let p = profile(&rec);
        assert_eq!(p.distinct_read_paths, 1, "one shape");
        assert_eq!(p.length_repeats, 0, "two resources, no repeat");
        assert_eq!(p.length_varied, 0);
    }

    #[test]
    fn two_resources_of_different_lengths_do_not_vary() {
        // The false-demotion direction, and the one that would have been easy
        // to ship: a normalized fingerprint would call two differently-sized
        // conversations a route whose responses are unstable.
        let rec = templated_recorder("/conversations/{id}");
        let len = |bytes| headers(&[("content-length", bytes)]);
        observe_read(&rec, "/conversations/a", None, 200, &len("42"));
        observe_read(&rec, "/conversations/b", None, 200, &len("9001"));
        let p = profile(&rec);
        assert_eq!(p.length_repeats, 0);
        assert_eq!(p.length_varied, 0, "different resources, not variance");
    }

    #[test]
    fn every_route_records_the_matcher_it_was_profiled_under() {
        let rec = ObserveRecorder::new(
            ObserveConfig::default(),
            [
                ("p", &prefix("/devices")),
                ("t", &template("/conversations/{id}")),
            ],
        );
        let document = rec.profile();
        assert_eq!(document.routes["p"].match_basis, "prefix:/devices");
        assert_eq!(
            document.routes["t"].match_basis,
            "template:/conversations/{id}"
        );
        // It survives serialization, which is where `suggest-routes` reads it.
        let json = serde_json::to_string(&document).unwrap();
        assert!(json.contains(r#""match_basis":"template:/conversations/{id}""#));
    }

    #[test]
    fn path_hashing_is_keyed_per_recorder() {
        // Not a collision test — collisions cannot be *observed* from outside.
        // What is observable, and what the fixed-key hasher lacked, is that the
        // mapping differs per process, so no attacker-chosen path pair is
        // reliably a collision on any given deployment.
        let a = recorder(ObserveConfig::default());
        let b = recorder(ObserveConfig::default());
        assert_ne!(
            a.hasher.hash_one("/same/path"),
            b.hasher.hash_one("/same/path"),
            "two recorders must not share a hash key"
        );
    }

    #[test]
    fn buckets_read_status_classes() {
        let rec = recorder(ObserveConfig::default());
        let empty = HeaderMap::new();
        for status in [200, 204, 303, 404] {
            observe_read(&rec, "/a", None, status, &empty);
        }
        let p = profile(&rec);
        assert_eq!(p.status_classes["2xx"], 2);
        assert_eq!(p.status_classes["3xx"], 1);
        assert_eq!(p.status_classes["4xx"], 1);
    }

    #[test]
    fn content_types_strip_parameters_and_bound() {
        let rec = recorder(ObserveConfig::default());
        observe_read(
            &rec,
            "/a",
            None,
            200,
            &headers(&[("content-type", "application/json; charset=utf-8")]),
        );
        observe_read(
            &rec,
            "/a",
            None,
            200,
            &headers(&[("content-type", "APPLICATION/JSON")]),
        );
        observe_read(
            &rec,
            "/a",
            None,
            200,
            &headers(&[("content-type", "text/html")]),
        );
        let p = profile(&rec);
        assert_eq!(
            p.content_types,
            BTreeSet::from(["application/json".to_string(), "text/html".to_string()])
        );
        assert!(!p.content_types_overflow);

        // The ninth essence overflows rather than being dropped silently.
        let rec = recorder(ObserveConfig::default());
        for n in 0..(MAX_CONTENT_TYPES + 1) {
            observe_read(
                &rec,
                "/a",
                None,
                200,
                &headers(&[("content-type", &format!("application/x-{n}"))]),
            );
        }
        let p = profile(&rec);
        assert_eq!(p.content_types.len(), MAX_CONTENT_TYPES);
        assert!(p.content_types_overflow);
    }

    #[test]
    fn counts_set_cookie_redirects_and_location() {
        let rec = recorder(ObserveConfig::default());
        observe_read(
            &rec,
            "/a",
            None,
            303,
            &headers(&[("location", "/next")]), // a bare redirect, no cookie
        );
        observe_read(&rec, "/a", None, 200, &headers(&[("set-cookie", "s=1")]));
        observe_read(&rec, "/a", None, 200, &HeaderMap::new());
        let p = profile(&rec);
        assert_eq!(p.redirect_reads, 1);
        assert_eq!(p.location_reads, 1);
        assert_eq!(p.set_cookie_reads, 1);
    }

    #[test]
    fn no_response_header_value_reaches_the_profile() {
        // Invariant 5 on the profile as an output surface, asserted at the unit
        // level and not only end to end. The content-type *essence* is recorded
        // by design; the full header value — parameters and all — is not, and
        // `Location`/`Set-Cookie` contribute counts only.
        let rec = recorder(ObserveConfig::default());
        let location = "/next?login_challenge=s3cret-token";
        let cookie = "session=abc123; Path=/; HttpOnly";
        let content_type = "text/html; charset=utf-8; boundary=zzz-boundary";
        observe_read(
            &rec,
            "/hop",
            None,
            303,
            &headers(&[
                ("location", location),
                ("set-cookie", cookie),
                ("content-type", content_type),
            ]),
        );
        let json = serde_json::to_string(&rec.profile()).unwrap();
        for leaked in [
            location,
            cookie,
            content_type,
            "s3cret-token",
            "abc123",
            "zzz-boundary",
            "/hop",
        ] {
            assert!(
                !json.contains(leaked),
                "the profile leaked {leaked:?}: {json}"
            );
        }
        assert!(json.contains("text/html"), "the essence is kept: {json}");
    }

    #[test]
    fn a_repeat_with_equal_length_is_a_stable_repeat() {
        let rec = recorder(ObserveConfig::default());
        let hdrs = headers(&[("content-length", "42")]);
        for _ in 0..3 {
            observe_read(&rec, "/a", Some("b=2&a=1"), 200, &hdrs);
        }
        let p = profile(&rec);
        assert_eq!(p.length_repeats, 2, "the first sighting proves nothing");
        assert_eq!(p.length_varied, 0);
        assert_eq!(p.length_missing, 0);
    }

    #[test]
    fn the_fingerprint_ignores_query_order_and_values() {
        let rec = recorder(ObserveConfig::default());
        let hdrs = headers(&[("content-length", "42")]);
        observe_read(&rec, "/a", Some("a=1&b=2"), 200, &hdrs);
        observe_read(&rec, "/a", Some("b=9&a=7"), 200, &hdrs);
        assert_eq!(
            profile(&rec).length_repeats,
            1,
            "reordered names with different values are the same fingerprint"
        );
    }

    #[test]
    fn a_repeat_with_a_different_length_also_varies() {
        let rec = recorder(ObserveConfig::default());
        observe_read(&rec, "/a", None, 200, &headers(&[("content-length", "42")]));
        observe_read(&rec, "/a", None, 200, &headers(&[("content-length", "43")]));
        let p = profile(&rec);
        assert_eq!(p.length_repeats, 1);
        assert_eq!(p.length_varied, 1);
    }

    #[test]
    fn a_missing_content_length_is_recorded_as_unassessable() {
        let rec = recorder(ObserveConfig::default());
        observe_read(&rec, "/a", None, 200, &HeaderMap::new());
        observe_read(&rec, "/a", None, 200, &HeaderMap::new());
        let p = profile(&rec);
        assert_eq!(p.length_missing, 2);
        assert_eq!(p.length_repeats, 0, "nothing was comparable");
    }

    #[test]
    fn only_successful_reads_enter_the_stability_map() {
        // The field shape this qualification exists for: an images route whose
        // every read 404s at one path, answered by a fixed-length error page.
        // Counting those repeats would hand the classifier the one affirmative
        // signal candidacy rests on, observed on a body no client is ever
        // served — so the map stays empty and the failures survive only in the
        // status classes, where the classifier's R8a reads them.
        let rec = recorder(ObserveConfig::default());
        let hdrs = headers(&[("content-type", "text/html"), ("content-length", "1024")]);
        for _ in 0..5 {
            observe_read(&rec, "/images/missing.png", None, 404, &hdrs);
        }
        observe_read(&rec, "/images/found.png", None, 200, &hdrs);
        let p = profile(&rec);
        assert_eq!(p.reads, 6);
        assert_eq!(p.status_classes["4xx"], 5, "the failures stay visible");
        assert_eq!(p.status_classes["2xx"], 1);
        assert_eq!(
            p.length_repeats, 0,
            "five repeats of a 404 page are not evidence about the route"
        );
        assert_eq!(p.length_varied, 0);
        assert_eq!(p.length_missing, 0);
        // The error responses are still described everywhere a *danger* rule
        // looks: the content type, the path count, and the status mix above.
        assert_eq!(p.distinct_read_paths, 2);
        assert!(p.content_types.contains("text/html"));
    }

    #[test]
    fn an_error_response_neither_repeats_nor_varies() {
        // Both directions asserted together: the error is not admitted as a
        // stable repeat, and it does not poison a genuine one either. A 200 of
        // one size and a 500 of another at the same fingerprint would otherwise
        // read as a route whose body varies — a false demotion, but still an
        // error being allowed to speak about the success path.
        let rec = recorder(ObserveConfig::default());
        let len = |bytes| headers(&[("content-length", bytes)]);
        observe_read(&rec, "/a", None, 200, &len("42"));
        observe_read(&rec, "/a", None, 500, &len("9001"));
        observe_read(&rec, "/a", None, 200, &len("42"));
        let p = profile(&rec);
        assert_eq!(p.length_repeats, 1, "the two 200s, and only those");
        assert_eq!(p.length_varied, 0);
        assert_eq!(p.length_missing, 0);
    }

    #[test]
    fn an_error_read_without_a_content_length_is_not_a_hole_in_the_evidence() {
        // `length_missing` is R11's signal that the *success* evidence has a
        // gap. A 404 with no length is not a gap in it; the route's failure is
        // R8a's to report.
        let rec = recorder(ObserveConfig::default());
        observe_read(&rec, "/a", None, 404, &HeaderMap::new());
        let p = profile(&rec);
        assert_eq!(p.length_missing, 0);
        assert_eq!(p.status_classes["4xx"], 1);
    }

    #[test]
    fn a_read_transport_error_is_counted_on_both_scopes_and_a_write_only_on_one() {
        // The classifier's R8a carve-out asks whether every *read* was withheld
        // by transport, so the read-scoped count is recorded alongside the
        // whole-route one. A route whose writes are timing out while its reads
        // are answered must not look like a route nobody has heard from.
        let rec = recorder(ObserveConfig::default());
        observe_synthesized(
            &rec,
            &Method::GET,
            "/a",
            None,
            502,
            ResponseOrigin::UpstreamSilent,
        );
        for _ in 0..3 {
            observe_synthesized(
                &rec,
                &Method::POST,
                "/a",
                None,
                504,
                ResponseOrigin::UpstreamSilent,
            );
        }
        observe_read(&rec, "/a", None, 404, &headers(&[("content-length", "9")]));
        let p = profile(&rec);
        assert_eq!(p.reads, 2);
        assert_eq!(p.writes, 3);
        assert_eq!(p.transport_errors, 4, "reads and writes alike");
        assert_eq!(p.read_transport_errors, 1, "one of the two reads");
    }

    #[test]
    fn a_locally_refused_read_never_reaches_the_stability_map() {
        // Belt and braces on the origin check: `Refused` returns before the
        // status class is consulted, so even a 2xx limen produced itself — a
        // shape no refusal path emits today — could not describe the route.
        let rec = recorder(ObserveConfig::default());
        for _ in 0..3 {
            observe_synthesized(&rec, &Method::GET, "/a", None, 200, ResponseOrigin::Refused);
        }
        let p = profile(&rec);
        assert_eq!(p.reads, 3);
        assert_eq!(p.status_classes["2xx"], 3, "the client did see a 200");
        assert_eq!(p.length_repeats, 0, "but limen wrote it, not the route");
        assert_eq!(p.length_missing, 0);
        assert_eq!(p.transport_errors, 0);
        assert_eq!(p.read_transport_errors, 0);
    }

    #[test]
    fn exceeding_max_fingerprints_flags_overflow() {
        // The load-bearing case: past the cap a new fingerprint must announce
        // itself, or variance beyond it goes unrecorded and the demotion the
        // classifier builds on silently stops firing.
        let rec = recorder(ObserveConfig {
            max_fingerprints: 2,
            ..ObserveConfig::default()
        });
        let hdrs = headers(&[("content-length", "42")]);
        for path in ["/a/1", "/a/2"] {
            observe_read(&rec, path, None, 200, &hdrs);
        }
        assert!(!profile(&rec).fingerprint_overflow);
        observe_read(&rec, "/a/3", None, 200, &hdrs);
        let p = profile(&rec);
        assert!(p.fingerprint_overflow);
        assert_eq!(p.length_repeats, 0, "the third shape was never admitted");
    }

    #[test]
    fn a_silent_upstream_is_counted_but_never_described() {
        let rec = recorder(ObserveConfig::default());
        for _ in 0..2 {
            observe_synthesized(
                &rec,
                &Method::GET,
                "/a",
                Some("q=1"),
                502,
                ResponseOrigin::UpstreamSilent,
            );
        }
        let p = profile(&rec);
        assert_eq!(p.observations, 2);
        assert_eq!(p.reads, 2);
        assert_eq!(p.transport_errors, 2);
        assert_eq!(p.status_classes["5xx"], 2);
        assert!(p.query_names.contains("q"), "the request still happened");
        assert_eq!(p.distinct_read_paths, 1);
        // The response was limen's, not the route's. Recording its plain-text
        // content type would describe a route that served nothing, and — the
        // dangerous one — its fixed-length error body would look like a
        // *stable* response and read as evidence the route is safe to shadow.
        assert!(p.content_types.is_empty());
        assert_eq!(p.length_repeats, 0);
        assert_eq!(p.length_missing, 0);
    }

    #[test]
    fn a_local_refusal_is_observed_but_is_not_a_transport_error() {
        // limen refused before contacting an upstream: the client saw a status,
        // so the route is not silent — but nothing failed upstream, and the
        // field would be a lie if it counted this.
        let rec = recorder(ObserveConfig::default());
        observe_synthesized(
            &rec,
            &Method::GET,
            "/a/../b",
            None,
            400,
            ResponseOrigin::Refused,
        );
        let p = profile(&rec);
        assert_eq!(p.observations, 1);
        assert_eq!(p.reads, 1);
        assert_eq!(p.transport_errors, 0);
        assert_eq!(p.status_classes["4xx"], 1);
        assert!(p.content_types.is_empty());
        assert_eq!(p.length_missing, 0);
    }

    #[test]
    fn a_silent_upstream_on_a_write_is_a_transport_error_too() {
        let rec = recorder(ObserveConfig::default());
        observe_synthesized(
            &rec,
            &Method::POST,
            "/a",
            None,
            504,
            ResponseOrigin::UpstreamSilent,
        );
        let p = profile(&rec);
        assert_eq!(p.writes, 1);
        assert_eq!(p.transport_errors, 1);
        assert!(p.status_classes.is_empty(), "status classes are reads only");
    }

    #[test]
    fn sampling_at_zero_records_nothing_and_at_one_records_everything() {
        let off = recorder(ObserveConfig {
            sample_rate: 0.0,
            ..ObserveConfig::default()
        });
        let on = recorder(ObserveConfig::default());
        for rec in [&off, &on] {
            for _ in 0..5 {
                observe_read(rec, "/a", None, 200, &HeaderMap::new());
            }
        }
        assert_eq!(
            profile(&off),
            RouteProfile {
                match_basis: "prefix:/".to_string(),
                ..RouteProfile::default()
            },
            "sampled out, but still enumerated with its basis"
        );
        assert_eq!(profile(&on).observations, 5);
    }

    /// Assert `needles` appear in `haystack` in exactly this order.
    fn in_order(haystack: &str, needles: &[&str]) {
        let mut previous = 0;
        for needle in needles {
            let at = haystack
                .find(needle)
                .unwrap_or_else(|| panic!("{needle:?} missing from {haystack}"));
            assert!(
                at > previous,
                "{needle:?} is out of canonical order in {haystack}"
            );
            previous = at;
        }
    }

    #[test]
    fn the_profile_document_is_canonically_ordered() {
        let root = prefix("/");
        let rec = ObserveRecorder::new(
            ObserveConfig::default(),
            [("z", &root), ("a", &root), ("m", &root), ("r", &root)],
        );
        // Insert every map/set out of order, so a non-`BTree` container would
        // render in insertion order and fail.
        for (method, status, query, content_type) in [
            (&Method::GET, 500, Some("zeta=1&alpha=2"), "text/plain"),
            (&Method::GET, 200, Some("mu=3"), "application/json"),
            (&Method::GET, 404, Some("beta=4"), "text/html"),
            (&Method::GET, 301, Some("alpha=5"), "application/xml"),
            (&Method::PUT, 200, None, "text/plain"),
            (&Method::DELETE, 200, None, "text/plain"),
        ] {
            observe(
                &rec,
                method,
                "/p",
                query,
                status,
                &headers(&[("content-type", content_type)]),
            );
        }
        let json = serde_json::to_string(&rec.profile()).unwrap();
        // Route keys...
        in_order(&json, [r#""a""#, r#""m""#, r#""r""#, r#""z""#].as_slice());
        // ...and every nested map and set inside the route that saw traffic.
        in_order(&json, ["DELETE", "GET", "PUT"].as_slice());
        in_order(&json, ["\"2xx\"", "\"3xx\"", "\"4xx\""].as_slice());
        in_order(&json, ["\"alpha\"", "\"mu\""].as_slice());
        in_order(
            &json,
            ["application/json", "application/xml", "text/html"].as_slice(),
        );
    }

    #[test]
    fn an_idle_profile_is_byte_stable_across_a_clock_tick() {
        // Two back-to-back scrapes would pass even with a second-resolution
        // timestamp in the document. The property `suggest-routes`' quiescence
        // poll actually depends on is that an *idle* profile does not change as
        // time passes, so the scrapes must straddle a plausible tick — one
        // second is the coarsest clock anyone would embed.
        let rec = recorder(ObserveConfig::default());
        observe_read(&rec, "/a", None, 200, &HeaderMap::new());
        let first = serde_json::to_string(&rec.profile()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        let second = serde_json::to_string(&rec.profile()).unwrap();
        assert_eq!(
            first, second,
            "an idle profile must not change as time passes"
        );
    }

    #[test]
    fn the_profile_reports_the_rate_the_recorder_applied() {
        // The rate travels with the document because the recorder is the only
        // party that knows it: a downstream tool handed a mismatched or
        // hand-edited config would otherwise classify a sampled profile as a
        // complete one, which is exactly what the classifier's R0 exists to
        // refuse.
        let rec = recorder(ObserveConfig {
            sample_rate: 0.25,
            ..ObserveConfig::default()
        });
        assert_eq!(rec.profile().sample_rate, 0.25);
        assert_eq!(
            recorder(ObserveConfig::default()).profile().sample_rate,
            1.0
        );
    }

    #[test]
    fn a_partial_route_object_does_not_deserialize() {
        // `deny_unknown_fields` does not deny *missing* fields, and a profile
        // missing its danger signals would zero-fill into a pristine-looking
        // route. Machine-produced input is read strictly.
        let partial = r#"{"sample_rate":1.0,"routes":{"r":{"observations":3,"reads":3}}}"#;
        assert!(serde_json::from_str::<ObserveProfile>(partial).is_err());
        let no_rate = r#"{"routes":{}}"#;
        assert!(serde_json::from_str::<ObserveProfile>(no_rate).is_err());
    }

    #[test]
    fn a_profile_without_a_match_basis_does_not_deserialize() {
        // A profile written by a binary that predates the field is exactly the
        // stale profile the field exists to catch, so it must fail at the parse
        // rather than default to a basis nobody recorded. Built by rendering a
        // real profile and deleting the key, so the fixture cannot rot into
        // one that was never a profile.
        let rec = recorder(ObserveConfig::default());
        observe_read(&rec, "/a", None, 200, &HeaderMap::new());
        let mut document: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&rec.profile()).unwrap()).unwrap();
        assert!(serde_json::from_value::<ObserveProfile>(document.clone()).is_ok());
        document["routes"]["r"]
            .as_object_mut()
            .unwrap()
            .remove("match_basis");
        assert!(serde_json::from_value::<ObserveProfile>(document).is_err());
    }

    #[test]
    fn a_profile_without_read_transport_errors_does_not_deserialize() {
        // The `match_basis` contract applied to the second field a classifier
        // rule reads directly: a profile from a binary that predates it would
        // zero-fill, and a zero there is the value that ARMS R8a's carve-out —
        // it would read "no read was withheld" about a profile that never said.
        // Same construction as the basis test, for the same anti-rot reason.
        let rec = recorder(ObserveConfig::default());
        observe_read(&rec, "/a", None, 200, &HeaderMap::new());
        let mut document: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&rec.profile()).unwrap()).unwrap();
        assert!(serde_json::from_value::<ObserveProfile>(document.clone()).is_ok());
        document["routes"]["r"]
            .as_object_mut()
            .unwrap()
            .remove("read_transport_errors");
        assert!(serde_json::from_value::<ObserveProfile>(document).is_err());
    }

    #[test]
    fn the_profile_round_trips() {
        // `suggest-routes --profile FILE` reads this document back.
        let rec = recorder(ObserveConfig::default());
        observe_read(
            &rec,
            "/a",
            Some("page=1"),
            200,
            &headers(&[
                ("content-type", "application/json"),
                ("content-length", "9"),
            ]),
        );
        let json = serde_json::to_string(&rec.profile()).unwrap();
        let parsed: ObserveProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, rec.profile());
    }
}
