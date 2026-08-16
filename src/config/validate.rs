//! Semantic validation of a loaded configuration (spec §5.3).
//!
//! This is more than a parse: it checks URL shapes, percentage ranges, timeout
//! sanity, route-ID uniqueness, per-mode required upstreams, contract reference
//! resolution, the contract-vs-inline conflict rule, JSONPath-subset compliance,
//! the `failover_safe` gate for non-idempotent failover routes, and budget
//! ranges. Every problem is collected (not just the first) and reported with the
//! offending field and route.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::compare::jsonpath;
use crate::config::model::{
    BudgetConfig, CircuitBreakerConfig, Config, DiffSinkConfig, FlagProviderKind, FlagsConfig,
    MetricsConfig, ObserveConfig, RolloutConfig, RouteConfig, RouteMatch, RouteMode,
    TimeoutsConfig, UpstreamTlsConfig,
};
use crate::contract::load as contract_load;
use crate::contract::model::{BehavioralRules, Contract};
use crate::health::endpoints::CONTROL_PLANE_RESERVED_PATHS;
use crate::observability::observe::{MAX_OBSERVE_BOUND, OBSERVE_PROFILE_PATH};
use crate::routing::matcher::PathMatcher;
use crate::routing::template::{self, CompiledTemplate};
use crate::verdict::{CANARY_ROUTE_ID, RESERVED_ROUTE_ID_PREFIX};

/// Loads each distinct contract file at most once per `validate()` call. A
/// contract typically holds many route entries, so without this the same file
/// would be re-read and re-parsed once per referencing route.
#[derive(Default)]
struct ContractCache {
    loaded: HashMap<PathBuf, Result<Contract, String>>,
}

impl ContractCache {
    fn get(&mut self, path: &Path) -> &Result<Contract, String> {
        self.loaded
            .entry(path.to_path_buf())
            .or_insert_with(|| contract_load::load_file(path).map_err(|e| e.to_string()))
    }

    /// Every successfully loaded contract, sorted by path for deterministic
    /// error ordering.
    fn loaded_contracts(&self) -> Vec<(&PathBuf, &Contract)> {
        let mut out: Vec<(&PathBuf, &Contract)> = self
            .loaded
            .iter()
            .filter_map(|(p, r)| r.as_ref().ok().map(|c| (p, c)))
            .collect();
        out.sort_by(|a, b| a.0.cmp(b.0));
        out
    }
}

/// HTTP methods Limen accepts in a route match.
const KNOWN_METHODS: &[&str] = &[
    "GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "TRACE", "CONNECT",
];

/// Methods that are *not* idempotent, so a `failover_to_legacy` route carrying
/// them must opt into replay explicitly (spec §5.3, §6.5).
const NON_IDEMPOTENT_METHODS: &[&str] = &["POST", "PATCH"];

/// Write methods a route may opt into shadowing via `comparison.shadow_methods`
/// (spec §6.1). Deliberately just `POST`: shadowing a write means replaying a
/// buffered body to a second upstream, and `POST` is the one verb the migration
/// use case (form/JSON submissions compared read-only against new) actually
/// needs. Reads are always eligible and are never listed here.
const SHADOWABLE_WRITE_METHODS: &[&str] = &["POST"];

/// A single semantic validation failure.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    /// Where the problem is (e.g. `routes[0] "get-device".legacy_upstream`).
    pub location: String,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.location, self.message)
    }
}

/// Accumulates validation errors with a location prefix.
struct Errors(Vec<ValidationError>);

impl Errors {
    fn push(&mut self, location: impl Into<String>, message: impl Into<String>) {
        self.0.push(ValidationError {
            location: location.into(),
            message: message.into(),
        });
    }
}

/// Validate a configuration. `base_dir` is the config file's directory, used to
/// resolve relative contract references.
pub fn validate(config: &Config, base_dir: &Path) -> Result<(), Vec<ValidationError>> {
    let mut errs = Errors(Vec::new());

    validate_socket_addr("server.listen_addr", &config.server.listen_addr, &mut errs);
    validate_socket_addr(
        "metrics.listen_addr",
        &config.metrics.listen_addr,
        &mut errs,
    );
    if config.server.graceful_shutdown_timeout_ms == 0 {
        errs.push(
            "server.graceful_shutdown_timeout_ms",
            "must be greater than 0",
        );
    }
    if config.server.request_body_limit_bytes == 0 {
        errs.push("server.request_body_limit_bytes", "must be greater than 0");
    }

    validate_tls(&config.upstream_tls, &mut errs);
    validate_flags(&config.flags, &mut errs);
    validate_diff_sink(config.diff_sink.as_ref(), &mut errs);
    validate_metrics_path(&config.metrics, &mut errs);
    validate_observe(config.observe.as_ref(), &config.metrics, &mut errs);

    let mut seen_ids: HashSet<&str> = HashSet::new();
    let mut contracts = ContractCache::default();
    for (i, route) in config.routes.iter().enumerate() {
        validate_route(i, route, base_dir, &mut seen_ids, &mut contracts, &mut errs);
    }
    validate_query_disjointness(config, &mut errs);
    validate_path_overlaps(config, &mut errs);

    // Validate each referenced contract's semantics once (version, service,
    // unique route ids), independent of how many routes point at it.
    for (path, contract) in contracts.loaded_contracts() {
        for issue in contract_load::validate_semantics(contract) {
            errs.push(format!("contract {}", path.display()), issue);
        }
    }

    if errs.0.is_empty() {
        Ok(())
    } else {
        Err(errs.0)
    }
}

fn validate_socket_addr(loc: &str, value: &str, errs: &mut Errors) {
    if value.parse::<SocketAddr>().is_err() {
        errs.push(
            loc,
            format!(
                "{value:?} is not a valid socket address (expected IP:port, e.g. 0.0.0.0:8080)"
            ),
        );
    }
}

fn validate_tls(tls: &UpstreamTlsConfig, errs: &mut Errors) {
    if let Some(path) = &tls.ca_bundle_path {
        if !path.exists() {
            errs.push(
                "upstream_tls.ca_bundle_path",
                format!("CA bundle {} does not exist", path.display()),
            );
        }
    }
}

fn validate_flags(flags: &FlagsConfig, errs: &mut Errors) {
    if flags.stale_ttl_ms == 0 {
        errs.push("flags.stale_ttl_ms", "must be greater than 0");
    }
    match flags.provider {
        FlagProviderKind::Static => {}
        FlagProviderKind::File => {
            if flags.file.path.as_os_str().is_empty() {
                errs.push("flags.file.path", "must be set when provider = file");
            }
            if flags.file.refresh_interval_ms == 0 {
                errs.push("flags.file.refresh_interval_ms", "must be greater than 0");
            }
        }
        FlagProviderKind::Redis => {
            validate_redis_url("flags.redis.url", &flags.redis.url, errs);
            if flags.redis.refresh_interval_ms == 0 {
                errs.push("flags.redis.refresh_interval_ms", "must be greater than 0");
            }
        }
    }
}

/// Validate the optional diff sink. Only the *shape* is checked: the directory
/// is created on the first mismatch, so requiring it (or its parent) to exist at
/// startup would make a fresh deploy fail validation for no reason.
fn validate_diff_sink(sink: Option<&DiffSinkConfig>, errs: &mut Errors) {
    let Some(sink) = sink else { return };
    if sink.dir.as_os_str().is_empty() {
        errs.push("diff_sink.dir", "must not be empty");
    }
}

/// Reject an operator-supplied `metrics.path` that collides with any control-
/// plane path the router registers unconditionally (`/health/live`,
/// `/health/ready`, `/debug/canary`). axum panics at router *build* time on a
/// duplicate route, so this is what turns that abort into a refuse-to-start
/// (invariant 7). `/observe/profile` is *not* checked here — it is only
/// registered when the observe block is present, so that collision is
/// validated separately in [`validate_observe`], conditional on the same
/// scoping the router uses.
fn validate_metrics_path(metrics: &MetricsConfig, errs: &mut Errors) {
    for reserved in CONTROL_PLANE_RESERVED_PATHS {
        if metrics.path == *reserved {
            errs.push(
                "metrics.path",
                format!(
                    "{reserved:?} is a fixed control-plane path and cannot also serve metrics \
                     — choose a different metrics.path"
                ),
            );
        }
    }
}

/// Validate the optional observe block. Absent = observation is off and there
/// is nothing to check, including the path collision below — an operator who
/// never asked to observe must not be told their metrics path is wrong.
fn validate_observe(observe: Option<&ObserveConfig>, metrics: &MetricsConfig, errs: &mut Errors) {
    let Some(observe) = observe else { return };

    validate_fraction("observe.sample_rate".to_string(), observe.sample_rate, errs);

    // Each bound caps a map keyed by live traffic, so `0` records nothing at
    // all rather than meaning "no limit". An operator writing it expects a
    // narrower profile, not an empty one.
    //
    // The ceiling matters just as much: invariant 6 requires these maps to be
    // bounded, and a cap an operator can set to `999999999` removes the bound
    // it exists to impose. See `MAX_OBSERVE_BOUND` for why four figures is the
    // line.
    for (field, value) in [
        ("max_query_names", observe.max_query_names),
        ("max_path_shapes", observe.max_path_shapes),
        ("max_fingerprints", observe.max_fingerprints),
    ] {
        if value == 0 {
            errs.push(format!("observe.{field}"), "must be greater than 0");
        } else if value > MAX_OBSERVE_BOUND {
            errs.push(
                format!("observe.{field}"),
                format!(
                    "must be at most {MAX_OBSERVE_BOUND} — this bounds a per-route map keyed by \
                     live traffic, and a larger value is not a bound"
                ),
            );
        }
    }

    // Same failure mode as `validate_metrics_path` (a router-build-time axum
    // panic on a duplicate route), but this route only exists when observe
    // does, so the check has to live here rather than in the unconditional set.
    if metrics.path == OBSERVE_PROFILE_PATH {
        errs.push(
            "metrics.path",
            format!(
                "{OBSERVE_PROFILE_PATH:?} is the observe profile endpoint and cannot also serve \
                 metrics — move metrics.path or remove the observe block"
            ),
        );
    }
}

fn validate_redis_url(loc: &str, value: &str, errs: &mut Errors) {
    match url::Url::parse(value) {
        Ok(u) if matches!(u.scheme(), "redis" | "rediss") && u.has_host() => {}
        Ok(u) => errs.push(
            loc,
            format!(
                "must be a redis:// or rediss:// URL with a host (got scheme {:?})",
                u.scheme()
            ),
        ),
        Err(e) => errs.push(loc, format!("{value:?} is not a valid URL: {e}")),
    }
}

fn validate_upstream_url(loc: &str, value: &str, errs: &mut Errors) {
    match url::Url::parse(value) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.has_host() => {}
        Ok(u) => errs.push(
            loc,
            format!(
                "must be an http(s) URL with a host (got scheme {:?})",
                u.scheme()
            ),
        ),
        Err(e) => errs.push(loc, format!("{value:?} is not a valid URL: {e}")),
    }
}

/// Where an error is reported: the route's config position, its id, and the
/// offending field. One producer, because this string is what an operator reads
/// out of `limen check` and what the tests match on — the per-route closure
/// below and the pairwise path checks must not drift apart.
fn route_loc(index: usize, id: &str, field: &str) -> String {
    format!("routes[{index}] {id:?}.{field}")
}

#[allow(clippy::too_many_arguments)]
fn validate_route<'a>(
    index: usize,
    route: &'a RouteConfig,
    base_dir: &Path,
    seen_ids: &mut HashSet<&'a str>,
    contracts: &mut ContractCache,
    errs: &mut Errors,
) {
    let loc = |field: &str| route_loc(index, &route.id, field);

    if route.id.trim().is_empty() {
        errs.push(format!("routes[{index}].id"), "must not be empty");
    } else if route.id.starts_with(RESERVED_ROUTE_ID_PREFIX) {
        // Reserved so limen's own records (today the debug sink canary, which
        // writes under CANARY_ROUTE_ID) can never be confused with a real
        // route's mismatches — `limen verdict` subtracts the namespace from
        // its mismatch totals and floors.
        errs.push(
            format!("routes[{index}].id"),
            format!(
                "route ids starting with {RESERVED_ROUTE_ID_PREFIX:?} are reserved for \
                 limen-internal records (e.g. {CANARY_ROUTE_ID:?}); rename the route"
            ),
        );
    } else if !seen_ids.insert(route.id.as_str()) {
        errs.push(
            format!("routes[{index}].id"),
            format!("duplicate route id {:?}", route.id),
        );
    }

    validate_match(&loc, route, errs);
    validate_upstreams(&loc, route, errs);
    validate_timeouts(&loc, &route.timeouts, errs);
    validate_comparison_operational(&loc, route, errs);
    validate_circuit_breaker(&loc, &route.circuit_breaker, errs);
    validate_rollout(&loc, route, errs);
    if let Some(budget) = &route.budget {
        validate_budget(&loc, budget, errs);
    }
    validate_failover_safety(&loc, route, errs);
    validate_behavioral_source(&loc, route, base_dir, contracts, errs);
}

fn validate_match(loc: &impl Fn(&str) -> String, route: &RouteConfig, errs: &mut Errors) {
    if route.r#match.methods.is_empty() {
        errs.push(loc("match.methods"), "must list at least one method");
    }
    for m in &route.r#match.methods {
        if !KNOWN_METHODS.contains(&m.to_ascii_uppercase().as_str()) {
            errs.push(loc("match.methods"), format!("unknown HTTP method {m:?}"));
        }
    }
    validate_path_expression(loc, &route.r#match, errs);
    validate_query_conditions(loc, &route.r#match, errs);
}

/// A match names its paths one way or the other, never both and never neither
/// (spec §5.2).
///
/// Both would leave matching with two answers to the same question. Neither
/// would be worse than a typo: with the field optional so a template route need
/// not write it, an omitted `path_prefix` that defaulted to `/` would silently
/// turn a mistyped route into a catch-all shadowing every path in the service.
fn validate_path_expression(loc: &impl Fn(&str) -> String, m: &RouteMatch, errs: &mut Errors) {
    match (&m.path_prefix, &m.path_template) {
        (Some(prefix), None) => {
            if prefix.is_empty() {
                errs.push(loc("match.path_prefix"), "must not be empty");
            } else if !prefix.starts_with('/') {
                errs.push(
                    loc("match.path_prefix"),
                    format!("must start with '/' (got {prefix:?})"),
                );
            }
        }
        (None, Some(template)) => {
            if let Err(e) = CompiledTemplate::parse(template) {
                errs.push(
                    loc("match.path_template"),
                    format!("path template {template:?} is invalid: {e}"),
                );
            }
        }
        (Some(_), Some(_)) => errs.push(
            loc("match"),
            "sets both path_prefix and path_template — a route matches on exactly one of the \
             two: a prefix (everything beneath it) or a template (one exact shape, with \
             `{param}` spanning a whole segment)",
        ),
        (None, None) => errs.push(
            loc("match"),
            "must set exactly one of path_prefix or path_template",
        ),
    }
}

/// Validate a route's own query conditions (spec §5.2): names are non-empty and
/// unique within a field, and no name appears in both fields — a route asking
/// for a parameter to be both present and absent could never match, so it is a
/// typo rather than an intent.
fn validate_query_conditions(loc: &impl Fn(&str) -> String, m: &RouteMatch, errs: &mut Errors) {
    validate_query_names(loc, "match.query_present", &m.query_present, errs);
    validate_query_names(loc, "match.query_absent", &m.query_absent, errs);
    for name in &m.query_present {
        if m.query_absent.contains(name) {
            errs.push(
                loc("match.query_present"),
                format!(
                    "query parameter {name:?} is also listed in match.query_absent — \
                     the route could never match"
                ),
            );
        }
    }
}

/// Validate one query-condition list: names are non-empty, unique, and spelled
/// as they will actually be compared.
///
/// Matching compares these config literals against request parameter names that
/// have already been percent-decoded (see [`crate::routing::matcher`]), so a name
/// carrying `%`, `+`, or edge whitespace could never equal a decoded name. Such a
/// route silently matches nothing — and "matches nothing" is the fail-open
/// direction here: the traffic it was meant to except falls through to whatever
/// sibling route would otherwise have shadowed it. Per safety invariant 7 these
/// are rejected at startup rather than normalized, so the operator fixes the
/// spelling instead of learning about it from a comparison that should never
/// have run.
fn validate_query_names(
    loc: &impl Fn(&str) -> String,
    field: &str,
    names: &[String],
    errs: &mut Errors,
) {
    let mut seen: HashSet<&str> = HashSet::new();
    for name in names {
        if name.trim().is_empty() {
            errs.push(loc(field), "query parameter names must not be empty");
            continue;
        }
        if name != name.trim() {
            errs.push(
                loc(field),
                format!(
                    "query parameter name {name:?} must not carry leading or trailing \
                     whitespace — it is compared literally against the request's decoded \
                     parameter names, so it could never match"
                ),
            );
        }
        if name.contains(['%', '+']) {
            errs.push(
                loc(field),
                format!(
                    "query parameter name {name:?} must be the literal decoded name: write \
                     `login_verifier`, not `login%5Fverifier` or `a+b` — request-side \
                     percent-decoding is applied before comparison, so an encoded spelling \
                     here could never match"
                ),
            );
        }
        if !seen.insert(name.as_str()) {
            errs.push(
                loc(field),
                format!("duplicate query parameter name {name:?}"),
            );
        }
    }
}

/// Reject any pair of query-conditioned routes that could both match the same
/// request (spec §5.2).
///
/// Matching breaks a prefix-length tie in favour of the query-conditioned route
/// — an unambiguous rule only while at most one conditioned route can match a
/// given request, which is what this check enforces. Rather than model
/// query-condition satisfiability, it is deliberately conservative: two conditioned
/// routes sharing a `path_prefix` and at least one method are accepted **only**
/// when they are *provably disjoint* — some parameter appears in one route's
/// `query_present` and the other's `query_absent`, so no single request can
/// satisfy both. Everything else (two `query_present` sets that a request could
/// carry together, a `query_present` / `query_absent` pair over unrelated
/// names) is an error, even where a cleverer analysis might prove it safe.
/// Routes on *different* prefixes never need this: longest prefix still decides.
///
/// Templated routes are out of scope here and handled by
/// [`validate_path_overlaps`], which applies this same disjointness rule to two
/// identical templates — and is stricter in one place: two identical
/// *unconditioned* prefixes are accepted (config order decides), while two
/// identical unconditioned templates are refused, there being no prefix length
/// left to order them by.
fn validate_query_disjointness(config: &Config, errs: &mut Errors) {
    for (i, a) in config.routes.iter().enumerate() {
        let Some(prefix_a) = a.r#match.path_prefix.as_deref() else {
            continue;
        };
        if !a.r#match.is_query_conditioned() {
            continue;
        }
        for (j, b) in config.routes.iter().enumerate().skip(i + 1) {
            if b.r#match.path_prefix.as_deref() != Some(prefix_a)
                || !b.r#match.is_query_conditioned()
                || !methods_overlap(&a.r#match.methods, &b.r#match.methods)
                || provably_disjoint(&a.r#match, &b.r#match)
            {
                continue;
            }
            errs.push(
                route_loc(j, &b.id, "match"),
                format!(
                    "query conditions overlap route {:?} (routes[{i}]) on path_prefix {:?}: \
                     two query-conditioned routes on the same prefix and method must be \
                     provably disjoint, i.e. some parameter must appear in one route's \
                     query_present and the other's query_absent so no request can satisfy \
                     both. Add such a parameter, or give the routes different prefixes or \
                     methods",
                    a.id, prefix_a
                ),
            );
        }
    }
}

/// Reject any pair of routes whose path expressions would make matching
/// arbitrary once the template tier is consulted ahead of the prefix tier
/// (spec §5.2; safety invariant 7 — refuse to start on ambiguity).
///
/// Only pairs whose methods overlap are considered; disjoint methods can never
/// compete. Prefix-vs-prefix pairs are not this function's business (longest
/// prefix decides, and [`validate_query_disjointness`] handles the equal-prefix
/// case). What is left is:
///
/// - **template vs template** — legal when the two cannot both match one path,
///   or when one is strictly narrower than the other (the matcher consults the
///   narrower first). Identical shapes fall back to the equal-prefix rules.
///   Co-matchable but incomparable (`/a/{x}/c` vs `/a/b/{y}`) is refused: any
///   order Limen picked would be a coin toss the operator never called.
/// - **template vs prefix** — legal when they cannot meet, or when every path
///   the template matches lies under the prefix (a refinement of a catch-all,
///   which is the reason templates exist). A template that takes *some* of a
///   prefix route's traffic and leaves the rest is refused, and a template that
///   overlaps a *query-conditioned* prefix route is refused unless the two are
///   provably query-disjoint — that conditioned route is usually the one
///   excepting an unsafe hop, and the template tier would otherwise take the
///   requests it exists to except.
fn validate_path_overlaps(config: &Config, errs: &mut Errors) {
    // The matcher's own view of each route's path, so this pass and matching
    // cannot disagree about what a match block names. A route whose own match is
    // already an error — both path fields, neither, or a template that does not
    // parse — becomes `None` and is skipped: it has been reported once already,
    // and pairing an expression Limen could not read with a second route would
    // only produce a consequential complaint about the same typo.
    let paths: Vec<Option<PathMatcher>> = config
        .routes
        .iter()
        .map(|r| PathMatcher::compile(&r.id, &r.r#match).ok())
        .collect();

    for (i, a) in config.routes.iter().enumerate() {
        for (j, b) in config.routes.iter().enumerate().skip(i + 1) {
            if !methods_overlap(&a.r#match.methods, &b.r#match.methods) {
                continue;
            }
            match (&paths[i], &paths[j]) {
                (Some(PathMatcher::Template(ta)), Some(PathMatcher::Template(tb))) => {
                    check_template_pair(i, a, ta, b, tb, route_loc(j, &b.id, "match"), errs)
                }
                (Some(PathMatcher::Template(ta)), Some(PathMatcher::Prefix(prefix))) => {
                    check_template_against_prefix(i, a, ta, j, b, prefix, errs)
                }
                (Some(PathMatcher::Prefix(prefix)), Some(PathMatcher::Template(tb))) => {
                    check_template_against_prefix(j, b, tb, i, a, prefix, errs)
                }
                // Prefix vs prefix (longest wins, and `validate_query_disjointness`
                // has the equal case), or a route already in error.
                _ => {}
            }
        }
    }
}

/// Two templated routes. Errors are located on the later of the two and name
/// both ids — either one can be the one to change.
fn check_template_pair(
    i: usize,
    a: &RouteConfig,
    ta: &CompiledTemplate,
    b: &RouteConfig,
    tb: &CompiledTemplate,
    location: String,
    errs: &mut Errors,
) {
    if !template::co_matchable(ta, tb) {
        return;
    }
    // Quoted as written, never Debug-printed: the operator has to match the
    // message against a line in their config file.
    let (ta_str, tb_str) = (ta.as_str(), tb.as_str());
    match (template::subsumes(ta, tb), template::subsumes(tb, ta)) {
        // The same shape, parameter names notwithstanding: nothing about the
        // path can order them, so the equal-prefix rules decide — with one
        // difference, below, where those rules have a prefix length to fall
        // back on and two identical templates have nothing.
        (true, true) => match (
            a.r#match.is_query_conditioned(),
            b.r#match.is_query_conditioned(),
        ) {
            (true, true) if !provably_disjoint(&a.r#match, &b.r#match) => errs.push(
                location,
                format!(
                    "query conditions overlap route {:?} (routes[{i}]) on the same path \
                     template {ta_str:?}: two query-conditioned routes on one shape and \
                     method must be provably disjoint, i.e. some parameter must appear \
                     in one route's query_present and the other's query_absent so no \
                     request can satisfy both",
                    a.id
                ),
            ),
            // Two identical *unconditioned* templates, unlike two identical
            // prefixes, are pure duplication: there is no length ordering left
            // to break the tie, so this is a typo rather than a precedence.
            (false, false) => errs.push(
                location,
                format!(
                    "path template {tb_str:?} matches exactly the same requests as route {:?} \
                     (routes[{i}]): parameter names do not narrow a template, so nothing \
                     distinguishes the two. Give one of them a query condition, a different \
                     shape, or different methods",
                    a.id
                ),
            ),
            // Exactly one conditioned, or both and provably disjoint: the
            // conditioned route wins, as it does at an equal prefix.
            _ => {}
        },
        // One is strictly narrower and is consulted first; matching has one
        // answer — unless the BROADER route carries a query condition. A
        // conditioned broad template is a carve-out across its whole shape
        // (the verifier-hop pattern), and the narrower unconditioned template
        // would steal exactly the requests the condition was written to
        // capture. Only provable query-disjointness makes that safe; the
        // reverse orientation (narrower conditioned, broader as fallback) is
        // ordinary refinement and needs no check.
        (true, false) | (false, true) => {
            let (narrow, broad) = if template::subsumes(ta, tb) {
                (a, b)
            } else {
                (b, a)
            };
            if broad.r#match.is_query_conditioned()
                && !narrow.r#match.is_query_conditioned()
                && !provably_disjoint(&narrow.r#match, &broad.r#match)
            {
                let (narrow_t, broad_t) = if std::ptr::eq(narrow, a) {
                    (ta_str, tb_str)
                } else {
                    (tb_str, ta_str)
                };
                errs.push(
                    location,
                    format!(
                        "path template {narrow_t:?} (route {:?}) is narrower than route {:?}'s \
                         query-conditioned template {broad_t:?} (routes[{i}]) and would win on \
                         every path it matches, stealing requests the query condition was \
                         written to capture. Make them provably query-disjoint (mirror the \
                         condition in query_absent on the narrower route) or narrow the \
                         conditioned route's shape",
                        narrow.id, broad.id
                    ),
                );
            }
        }
        (false, false) => {
            let witness = template::witness_path(ta, tb);
            errs.push(
                location,
                format!(
                    "path template {tb_str:?} overlaps route {:?}'s template {ta_str:?} \
                     (routes[{i}]) — {witness:?} matches both, and neither template is narrower \
                     than the other, so which one wins would be an accident of config order. \
                     Rewrite one of them so it is either disjoint from or strictly narrower than \
                     the other",
                    a.id
                ),
            )
        }
    }
}

/// A templated route against a prefix route. `t_index`/`p_index` are the two
/// routes' config positions; the error lands on whichever comes later.
fn check_template_against_prefix(
    t_index: usize,
    t_route: &RouteConfig,
    t: &CompiledTemplate,
    p_index: usize,
    p_route: &RouteConfig,
    prefix: &str,
    errs: &mut Errors,
) {
    if !template::intersects_prefix(t, prefix) {
        return;
    }
    // As written, not Debug-printed (see `check_template_pair`).
    let t_str = t.as_str();
    let (later, later_index) = if t_index > p_index {
        (t_route, t_index)
    } else {
        (p_route, p_index)
    };
    let location = route_loc(later_index, &later.id, "match");
    if p_route.r#match.is_query_conditioned() {
        if provably_disjoint(&t_route.r#match, &p_route.r#match) {
            return;
        }
        errs.push(
            location,
            format!(
                "path template {t_str:?} (route {:?}, routes[{t_index}]) overlaps the \
                 query-conditioned route {:?} (routes[{p_index}]) on path_prefix {prefix:?}: \
                 templates are matched before prefixes, so the template would take the very \
                 requests that conditioned route exists to except. Make the two provably \
                 disjoint — some parameter must appear in one route's query_present and the \
                 other's query_absent — or give them different paths or methods",
                t_route.id, p_route.id
            ),
        );
        return;
    }
    if template::contained_in_prefix(t, prefix) {
        return;
    }
    errs.push(
        location,
        format!(
            "path template {t_str:?} (route {:?}, routes[{t_index}]) overlaps path_prefix \
             {prefix:?} (route {:?}, routes[{p_index}]) without lying entirely beneath it: \
             templates are matched before prefixes, so this pair would split that prefix \
             route's traffic on a boundary neither route states. Rewrite the prefix route as \
             an all-literal path_template, or change one of the two so every path the template \
             matches falls under the prefix",
            t_route.id, p_route.id
        ),
    );
}

/// Whether the two routes' conditions can be *proven* mutually exclusive by the
/// one rule the disjointness check recognizes (see
/// [`validate_query_disjointness`]).
fn provably_disjoint(a: &RouteMatch, b: &RouteMatch) -> bool {
    a.query_present.iter().any(|n| b.query_absent.contains(n))
        || b.query_present.iter().any(|n| a.query_absent.contains(n))
}

fn methods_overlap(a: &[String], b: &[String]) -> bool {
    a.iter()
        .any(|m| b.iter().any(|other| other.eq_ignore_ascii_case(m)))
}

fn validate_upstreams(loc: &impl Fn(&str) -> String, route: &RouteConfig, errs: &mut Errors) {
    let mode = route.mode;
    check_upstream(
        loc,
        "legacy_upstream",
        &route.legacy_upstream,
        mode.uses_legacy(),
        mode,
        errs,
    );
    check_upstream(
        loc,
        "new_upstream",
        &route.new_upstream,
        mode.uses_new(),
        mode,
        errs,
    );
}

/// Validate one upstream: it must be a well-formed http(s) URL when present, and
/// must be present when the mode requires it.
fn check_upstream(
    loc: &impl Fn(&str) -> String,
    field: &str,
    value: &Option<String>,
    required: bool,
    mode: RouteMode,
    errs: &mut Errors,
) {
    match value {
        Some(url) => validate_upstream_url(&loc(field), url, errs),
        None if required => errs.push(loc(field), format!("required for mode {:?}", mode.as_str())),
        None => {}
    }
}

fn validate_timeouts(loc: &impl Fn(&str) -> String, t: &TimeoutsConfig, errs: &mut Errors) {
    if t.primary_ms == 0 {
        errs.push(loc("timeouts.primary_ms"), "must be greater than 0");
    }
    if t.shadow_ms == 0 {
        errs.push(loc("timeouts.shadow_ms"), "must be greater than 0");
    }
}

/// Validate that a value is a fraction within `0.0..=1.0`. A range `contains`
/// check already rejects NaN (all comparisons with NaN are false).
fn validate_fraction(location: String, value: f64, errs: &mut Errors) {
    if !(0.0..=1.0).contains(&value) {
        errs.push(location, format!("must be within 0.0..=1.0 (got {value})"));
    }
}

fn validate_comparison_operational(
    loc: &impl Fn(&str) -> String,
    route: &RouteConfig,
    errs: &mut Errors,
) {
    validate_fraction(
        loc("comparison.sample_rate"),
        route.comparison.sample_rate,
        errs,
    );
    // An explicit positive floor on a comparison-disabled route could never
    // be met, and `limen verdict` would silently exclude it from the floors
    // check — the operator believes the route is verified, it never was.
    // Like an inert `shadow_methods` listing, this refuses to start.
    if !route.comparison.enabled && route.comparison.min_comparisons.is_some_and(|m| m > 0) {
        errs.push(
            loc("comparison.min_comparisons"),
            "a positive verdict floor on a comparison-disabled route can never be met — \
             enable comparison or set min_comparisons: 0",
        );
    }
    validate_shadow_methods(loc, route, errs);
}

/// Validate the per-route write-shadowing opt-in (spec §6.1). Every rejection
/// here is a *silently inert* setting: an operator who writes `shadow_methods`
/// expects those writes to be shadowed, so a listing that can never take effect
/// must refuse to start rather than look configured.
fn validate_shadow_methods(loc: &impl Fn(&str) -> String, route: &RouteConfig, errs: &mut Errors) {
    let opted_in = &route.comparison.shadow_methods;
    if opted_in.is_empty() {
        return;
    }
    let field = loc("comparison.shadow_methods");
    let matched: Vec<String> = route
        .r#match
        .methods
        .iter()
        .map(|m| m.to_ascii_uppercase())
        .collect();
    for method in opted_in {
        let upper = method.to_ascii_uppercase();
        if !SHADOWABLE_WRITE_METHODS.contains(&upper.as_str()) {
            errs.push(
                field.clone(),
                format!(
                    "{method:?} cannot be opted into shadowing — only {SHADOWABLE_WRITE_METHODS:?} \
                     may be listed (GET/HEAD are always eligible and must not be listed)"
                ),
            );
        } else if !matched.contains(&upper) {
            errs.push(
                field.clone(),
                format!("{method:?} is not in match.methods, so the route never sees it"),
            );
        }
    }
    if route.mode != RouteMode::ShadowLegacyPrimary {
        errs.push(
            field.clone(),
            format!(
                "only mode shadow_legacy_primary shadows requests (got {:?})",
                route.mode.as_str()
            ),
        );
    }
    if !route.comparison.enabled {
        errs.push(field, "requires comparison.enabled: true");
    }
}

fn validate_circuit_breaker(
    loc: &impl Fn(&str) -> String,
    cb: &CircuitBreakerConfig,
    errs: &mut Errors,
) {
    validate_fraction(
        loc("circuit_breaker.failure_rate_threshold"),
        cb.failure_rate_threshold,
        errs,
    );
    if cb.enabled {
        if cb.min_requests == 0 {
            errs.push(
                loc("circuit_breaker.min_requests"),
                "must be greater than 0 when enabled",
            );
        }
        if cb.open_duration_ms == 0 {
            errs.push(
                loc("circuit_breaker.open_duration_ms"),
                "must be greater than 0 when enabled",
            );
        }
        if cb.half_open_max_requests == 0 {
            errs.push(
                loc("circuit_breaker.half_open_max_requests"),
                "must be greater than 0 when enabled",
            );
        }
    }
}

fn validate_rollout(loc: &impl Fn(&str) -> String, route: &RouteConfig, errs: &mut Errors) {
    match (&route.rollout, route.mode) {
        (None, RouteMode::PercentageSplit) => {
            errs.push(loc("rollout"), "required for mode percentage_split");
        }
        (Some(r), _) => validate_rollout_block(loc, r, errs),
        (None, _) => {}
    }
}

fn validate_rollout_block(loc: &impl Fn(&str) -> String, r: &RolloutConfig, errs: &mut Errors) {
    if r.percentage_flag.trim().is_empty() {
        errs.push(loc("rollout.percentage_flag"), "must not be empty");
    }
    let p = r.default_percentage;
    if !(0.0..=100.0).contains(&p) || p.is_nan() {
        errs.push(
            loc("rollout.default_percentage"),
            format!("must be within 0..=100 (got {p})"),
        );
    }
}

fn validate_budget(loc: &impl Fn(&str) -> String, b: &BudgetConfig, errs: &mut Errors) {
    if b.max_new_p95_latency_ratio.is_nan() || b.max_new_p95_latency_ratio <= 0.0 {
        errs.push(
            loc("budget.max_new_p95_latency_ratio"),
            "must be a positive number",
        );
    }
    if b.max_new_error_rate_ratio.is_nan() || b.max_new_error_rate_ratio <= 0.0 {
        errs.push(
            loc("budget.max_new_error_rate_ratio"),
            "must be a positive number",
        );
    }
    validate_fraction(loc("budget.max_mismatch_rate"), b.max_mismatch_rate, errs);
}

fn validate_failover_safety(loc: &impl Fn(&str) -> String, route: &RouteConfig, errs: &mut Errors) {
    if route.mode != RouteMode::FailoverToLegacy || route.failover_safe {
        return;
    }
    let offending: Vec<String> = route
        .r#match
        .methods
        .iter()
        .map(|m| m.to_ascii_uppercase())
        .filter(|m| NON_IDEMPOTENT_METHODS.contains(&m.as_str()))
        .collect();
    if !offending.is_empty() {
        errs.push(
            loc("failover_safe"),
            format!(
                "must be true for a failover_to_legacy route with non-idempotent methods {offending:?} \
                 — auto-failover would risk replaying a side-effecting request"
            ),
        );
    }
}

/// Validate the route's behavioral source: at most one of (contract ref, inline
/// rules), and every JSONPath it references is in the supported subset.
fn validate_behavioral_source(
    loc: &impl Fn(&str) -> String,
    route: &RouteConfig,
    base_dir: &Path,
    contracts: &mut ContractCache,
    errs: &mut Errors,
) {
    let has_contract = route.contract.is_some();
    let has_inline = route.comparison.has_inline_behavioral();

    if has_contract && has_inline {
        errs.push(
            loc("comparison"),
            "a route may reference a contract or inline behavioral rules, not both",
        );
        return;
    }

    if let Some(reference) = &route.contract {
        validate_contract_reference(loc, reference, base_dir, contracts, errs);
    } else if has_inline {
        let inline = route.comparison.inline_behavioral();
        validate_jsonpaths(loc, "comparison", &inline, errs);
        // Inline rules speak the same vocabulary as a contract layer, so they
        // are subject to the same `compare_headers`-vs-block conflict (§4.2);
        // contract-referencing routes are covered by `validate_semantics`.
        for (header, block) in contract_load::header_dimension_conflicts(&[&inline]) {
            errs.push(
                loc("comparison"),
                contract_load::header_dimension_conflict_message(header, block),
            );
        }
    }
}

fn validate_contract_reference(
    loc: &impl Fn(&str) -> String,
    reference: &str,
    base_dir: &Path,
    contracts: &mut ContractCache,
    errs: &mut Errors,
) {
    let parsed = match contract_load::parse_ref(reference) {
        Ok(p) => p,
        Err(e) => {
            errs.push(loc("contract"), e.to_string());
            return;
        }
    };
    let path = contract_load::resolve_path(base_dir, &parsed.file);
    let contract = match contracts.get(&path) {
        Ok(c) => c,
        Err(message) => {
            errs.push(loc("contract"), message.clone());
            return;
        }
    };
    let Some(route_entry) = contract.route(&parsed.route_id) else {
        errs.push(
            loc("contract"),
            format!(
                "contract {} has no route {:?}",
                path.display(),
                parsed.route_id
            ),
        );
        return;
    };

    // Validate the JSONPaths that this route will actually use: service
    // defaults plus the referenced route's overrides.
    validate_jsonpaths(loc, "contract:defaults", &contract.defaults, errs);
    if let Some(comparison) = &route_entry.comparison {
        validate_jsonpaths(loc, "contract:route", comparison, errs);
    }
}

fn validate_jsonpaths(
    loc: &impl Fn(&str) -> String,
    source: &str,
    rules: &BehavioralRules,
    errs: &mut Errors,
) {
    for (field, path) in rules.json_paths() {
        if let Err(e) = jsonpath::parse(path) {
            errs.push(
                loc(&format!("{source}.{field}")),
                format!("JSONPath {path:?} is outside the supported subset: {e}"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn base() -> PathBuf {
        PathBuf::from(".")
    }

    fn parse(yaml: &str) -> Config {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn errors(yaml: &str) -> Vec<ValidationError> {
        validate(&parse(yaml), &base()).unwrap_err()
    }

    fn locations(errs: &[ValidationError]) -> Vec<&str> {
        errs.iter().map(|e| e.location.as_str()).collect()
    }

    const VALID: &str = r#"
routes:
  - id: get-device
    match: { methods: ["GET"], path_prefix: "/devices/" }
    legacy_upstream: "https://legacy.internal"
    new_upstream: "https://new.internal"
    mode: shadow_legacy_primary
    comparison: { enabled: true, sample_rate: 0.1, max_body_bytes: 262144 }
"#;

    #[test]
    fn valid_minimal_config_passes() {
        assert!(validate(&parse(VALID), &base()).is_ok());
        assert!(validate(&Config::default(), &base()).is_ok());
    }

    #[test]
    fn invalid_upstream_url_is_caught() {
        let errs = errors(
            r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "not a url"
    new_upstream: "ftp://nope.internal"
    mode: shadow_legacy_primary
"#,
        );
        assert!(locations(&errs)
            .iter()
            .any(|l| l.contains("legacy_upstream")));
        assert!(locations(&errs).iter().any(|l| l.contains("new_upstream")));
    }

    #[test]
    fn percentage_out_of_range_is_caught() {
        let errs = errors(
            r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: percentage_split
    rollout:
      percentage_flag: "f"
      default_percentage: 150
      assignment_key: { fallback: request_random }
"#,
        );
        assert!(locations(&errs)
            .iter()
            .any(|l| l.contains("default_percentage")));
    }

    #[test]
    fn missing_upstream_for_mode_is_caught() {
        let errs = errors(
            r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "https://l"
    mode: shadow_legacy_primary
"#,
        );
        assert!(errs
            .iter()
            .any(|e| e.location.contains("new_upstream") && e.message.contains("required")));
    }

    #[test]
    fn zero_timeout_is_caught() {
        let errs = errors(
            r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: shadow_legacy_primary
    timeouts: { primary_ms: 0, shadow_ms: 0 }
"#,
        );
        assert!(locations(&errs).iter().any(|l| l.contains("primary_ms")));
        assert!(locations(&errs).iter().any(|l| l.contains("shadow_ms")));
    }

    #[test]
    fn duplicate_route_ids_are_caught() {
        let errs = errors(
            r#"
routes:
  - id: dup
    match: { methods: ["GET"], path_prefix: "/a" }
    legacy_upstream: "https://l"
    mode: legacy_only
  - id: dup
    match: { methods: ["GET"], path_prefix: "/b" }
    legacy_upstream: "https://l"
    mode: legacy_only
"#,
        );
        assert!(errs
            .iter()
            .any(|e| e.message.contains("duplicate route id")));
    }

    #[test]
    fn a_positive_floor_on_a_disabled_route_is_rejected() {
        // Without this, the route silently vanishes from `limen verdict`'s
        // floors check while looking floored in the config (fail-open).
        let errs = errors(
            r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/a" }
    legacy_upstream: "https://l"
    mode: legacy_only
    comparison: { enabled: false, min_comparisons: 500 }
"#,
        );
        assert!(
            errs.iter().any(|e| e.message.contains("never be met")),
            "{errs:?}"
        );
        // The explicit opt-out and the plain default stay fully valid.
        for comparison in [
            "{ enabled: false, min_comparisons: 0 }",
            "{ enabled: false }",
        ] {
            let config = parse(&format!(
                r#"
routes:
  - id: r
    match: {{ methods: ["GET"], path_prefix: "/a" }}
    legacy_upstream: "https://l"
    mode: legacy_only
    comparison: {comparison}
"#
            ));
            assert!(
                validate(&config, &base()).is_ok(),
                "{comparison} should be valid"
            );
        }
    }

    #[test]
    fn reserved_route_id_prefix_is_rejected() {
        // The canary id itself and any other `__` id: limen owns the namespace,
        // so a verdict's "these are not real mismatches" subtraction is exact.
        for id in ["__limen_canary__", "__anything"] {
            let errs = errors(&format!(
                r#"
routes:
  - id: {id}
    match: {{ methods: ["GET"], path_prefix: "/a" }}
    legacy_upstream: "https://l"
    mode: legacy_only
"#
            ));
            assert!(
                errs.iter().any(|e| e.message.contains("reserved")),
                "{id}: {errs:?}"
            );
        }
    }

    #[test]
    fn unknown_method_is_caught() {
        let errs = errors(
            r#"
routes:
  - id: r
    match: { methods: ["FETCH"], path_prefix: "/" }
    legacy_upstream: "https://l"
    mode: legacy_only
"#,
        );
        assert!(errs
            .iter()
            .any(|e| e.message.contains("unknown HTTP method")));
    }

    #[test]
    fn contract_and_inline_conflict_is_caught() {
        let errs = errors(
            r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: shadow_legacy_primary
    contract: "./c.contract.yaml#r"
    comparison:
      enabled: true
      sample_rate: 1.0
      max_body_bytes: 1024
      json: { ignore_paths: ["$.ts"] }
"#,
        );
        assert!(errs.iter().any(|e| e.message.contains("not both")));
    }

    #[test]
    fn out_of_subset_inline_jsonpath_is_caught() {
        let errs = errors(
            r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: shadow_legacy_primary
    comparison:
      enabled: true
      sample_rate: 1.0
      max_body_bytes: 1024
      json: { ignore_paths: ["$.items[0].id"] }
"#,
        );
        assert!(errs
            .iter()
            .any(|e| e.message.contains("outside the supported subset")));
    }

    #[test]
    fn inline_compare_headers_conflicting_with_a_block_is_caught() {
        let errs = errors(
            r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: shadow_legacy_primary
    comparison:
      enabled: true
      sample_rate: 1.0
      max_body_bytes: 1024
      compare_headers: ["Content-Type", "Set-Cookie", "location"]
      set_cookie: { compare_values: presence }
      location: {}
"#,
        );
        let conflicts: Vec<&str> = errs
            .iter()
            .filter(|e| e.message.contains("separate comparison dimension"))
            .map(|e| e.message.as_str())
            .collect();
        assert_eq!(conflicts.len(), 2, "{errs:?}");
        assert!(conflicts.iter().any(|m| m.contains("`set_cookie`")));
        assert!(conflicts.iter().any(|m| m.contains("`location`")));
        // The location prefix names the offending route.
        assert!(errs
            .iter()
            .filter(|e| e.message.contains("separate comparison dimension"))
            .all(|e| e.location.contains("\"r\"")));
    }

    #[test]
    fn inline_compare_headers_set_cookie_without_a_block_is_caught() {
        // No `set_cookie` block: still rejected, because the generic header
        // path would compare one value of a multi-cookie response. `location`
        // on its own stays legal.
        let errs = errors(
            r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: shadow_legacy_primary
    comparison:
      enabled: true
      sample_rate: 1.0
      max_body_bytes: 1024
      compare_headers: ["set-cookie", "location"]
"#,
        );
        assert_eq!(errs.len(), 1, "{errs:?}");
        let conflicts: Vec<&str> = errs
            .iter()
            .filter(|e| e.message.contains("separate comparison dimension"))
            .map(|e| e.message.as_str())
            .collect();
        assert_eq!(conflicts.len(), 1, "{errs:?}");
        assert!(conflicts[0].contains("\"set-cookie\""));
        assert!(conflicts[0].contains("`set_cookie` block"));
    }

    #[test]
    fn inline_blocks_without_the_header_entry_are_fine() {
        let config = parse(
            r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: shadow_legacy_primary
    comparison:
      enabled: true
      sample_rate: 1.0
      max_body_bytes: 1024
      compare_headers: ["content-type"]
      set_cookie: { ignore_cookies: ["csrf_token"] }
      location: { origin: ignore }
"#,
        );
        assert!(validate(&config, &base()).is_ok());
    }

    #[test]
    fn failover_to_legacy_with_post_requires_failover_safe() {
        let errs = errors(
            r#"
routes:
  - id: r
    match: { methods: ["POST"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: failover_to_legacy
"#,
        );
        assert!(errs.iter().any(|e| e.location.contains("failover_safe")));

        // With failover_safe: true, the same route validates.
        let ok = r#"
routes:
  - id: r
    match: { methods: ["POST"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: failover_to_legacy
    failover_safe: true
"#;
        assert!(validate(&parse(ok), &base()).is_ok());
    }

    /// Widening the mid-flight replay to `percentage_split` (plan 016) moves the
    /// proxy's gate, not this validation rule. The *requirement* to set the flag
    /// exists because `failover_to_legacy` sends every request to new whether or
    /// not the operator thought about failover; a split route is an explicit,
    /// per-percentage act already, so the flag stays optional there and means
    /// only what it always meant — an idempotence attestation. A split route is
    /// therefore valid with the flag and without it, whatever methods it matches.
    #[test]
    fn percentage_split_never_requires_failover_safe() {
        for failover_safe in [false, true] {
            let yaml = format!(
                r#"
routes:
  - id: r
    match: {{ methods: ["POST", "PATCH"], path_prefix: "/" }}
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: percentage_split
    failover_safe: {failover_safe}
    rollout:
      percentage_flag: "f"
      default_percentage: 0
      assignment_key: {{ fallback: request_random }}
"#
            );
            assert!(
                validate(&parse(&yaml), &base()).is_ok(),
                "percentage_split with failover_safe: {failover_safe} must validate",
            );
        }
    }

    /// The other half of the parity: the rule that *does* bite still bites, and
    /// still only for `failover_to_legacy`.
    #[test]
    fn only_failover_to_legacy_demands_the_attestation() {
        let post = |mode: &str, rollout: &str| {
            format!(
                r#"
routes:
  - id: r
    match: {{ methods: ["POST"], path_prefix: "/" }}
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: {mode}
{rollout}"#
            )
        };
        let demands = |yaml: &str| {
            validate(&parse(yaml), &base())
                .err()
                .is_some_and(|errs| errs.iter().any(|e| e.location.contains("failover_safe")))
        };
        assert!(demands(&post("failover_to_legacy", "")));
        assert!(!demands(&post(
            "percentage_split",
            "    rollout:\n      percentage_flag: \"f\"\n      default_percentage: 0\n      assignment_key: { fallback: request_random }\n"
        )));
        assert!(!demands(&post("new_only", "")));
        assert!(!demands(&post("legacy_only", "")));
    }

    /// The supported opt-in: `POST` on a shadowing route with comparison on.
    #[test]
    fn shadow_methods_post_on_a_shadow_route_is_accepted() {
        let ok = r#"
routes:
  - id: r
    match: { methods: ["GET", "POST"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: shadow_legacy_primary
    comparison: { enabled: true, sample_rate: 1.0, shadow_methods: ["POST"] }
"#;
        assert!(validate(&parse(ok), &base()).is_ok());
    }

    #[test]
    fn shadow_methods_rejects_anything_but_post() {
        let errs = errors(
            r#"
routes:
  - id: r
    match: { methods: ["GET", "DELETE"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: shadow_legacy_primary
    comparison: { enabled: true, sample_rate: 1.0, shadow_methods: ["DELETE", "GET"] }
"#,
        );
        let messages: Vec<&str> = errs
            .iter()
            .filter(|e| e.location.contains("comparison.shadow_methods"))
            .map(|e| e.message.as_str())
            .collect();
        assert_eq!(messages.len(), 2, "{errs:?}");
        assert!(messages.iter().any(|m| m.contains("\"DELETE\"")));
        // A read must not be listed: it is eligible anyway, and listing it
        // suggests the operator expected the field to *restrict* eligibility.
        assert!(messages.iter().any(|m| m.contains("\"GET\"")));
    }

    /// Every remaining rejection is about a listing that could never take
    /// effect: a mode that does not shadow, comparison switched off, or a
    /// method the route does not even match.
    #[test]
    fn shadow_methods_on_an_inert_route_is_caught() {
        for (yaml, expected) in [
            (
                r#"
routes:
  - id: r
    match: { methods: ["POST"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: failover_to_legacy
    failover_safe: true
    comparison: { enabled: true, sample_rate: 1.0, shadow_methods: ["POST"] }
"#,
                "shadow_legacy_primary",
            ),
            (
                r#"
routes:
  - id: r
    match: { methods: ["POST"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: shadow_legacy_primary
    comparison: { enabled: false, shadow_methods: ["POST"] }
"#,
                "comparison.enabled",
            ),
            (
                r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: shadow_legacy_primary
    comparison: { enabled: true, sample_rate: 1.0, shadow_methods: ["POST"] }
"#,
                "not in match.methods",
            ),
        ] {
            let errs = errors(yaml);
            assert!(
                errs.iter()
                    .any(|e| e.location.contains("shadow_methods") && e.message.contains(expected)),
                "expected {expected:?} in {errs:?}"
            );
        }
    }

    #[test]
    fn failover_to_legacy_with_get_is_fine_without_flag() {
        let ok = r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: failover_to_legacy
"#;
        assert!(validate(&parse(ok), &base()).is_ok());
    }

    /// The field-motivated split (spec §5.2): the verifier hops relay
    /// uncompared, everything else on the path stays compared. The pair is
    /// provably disjoint on `login_verifier`, so it validates.
    #[test]
    fn provably_disjoint_query_conditioned_routes_are_accepted() {
        let ok = r#"
routes:
  - id: oauth-verifier
    match:
      methods: ["GET"]
      path_prefix: "/oauth2/auth"
      query_present: ["login_verifier"]
    legacy_upstream: "https://l"
    mode: legacy_only
  - id: oauth-authorize
    match:
      methods: ["GET"]
      path_prefix: "/oauth2/auth"
      query_absent: ["login_verifier"]
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: shadow_legacy_primary
    comparison: { enabled: true, sample_rate: 1.0 }
"#;
        assert!(validate(&parse(ok), &base()).is_ok());
    }

    #[test]
    fn empty_or_duplicated_query_names_are_caught() {
        let errs = errors(
            r#"
routes:
  - id: r
    match:
      methods: ["GET"]
      path_prefix: "/x"
      query_present: ["a", "a", ""]
      query_absent: ["b", "b"]
    legacy_upstream: "https://l"
    mode: legacy_only
"#,
        );
        let present: Vec<&str> = errs
            .iter()
            .filter(|e| e.location.contains("match.query_present"))
            .map(|e| e.message.as_str())
            .collect();
        assert_eq!(present.len(), 2, "{errs:?}");
        assert!(present.iter().any(|m| m.contains("duplicate")));
        assert!(present.iter().any(|m| m.contains("must not be empty")));
        assert!(errs
            .iter()
            .any(|e| e.location.contains("match.query_absent") && e.message.contains("duplicate")));
    }

    /// A name that cannot survive the request-side percent-decoding is rejected,
    /// not normalized: it would otherwise validate, match nothing, and let the
    /// traffic it was meant to except fall through to a shadowing sibling.
    #[test]
    fn query_names_that_could_never_match_a_decoded_name_are_caught() {
        for (name, expected) in [
            ("login_verifier ", "whitespace"),
            (" login_verifier", "whitespace"),
            ("login%5Fverifier", "literal decoded name"),
            ("a+b", "literal decoded name"),
        ] {
            let errs = errors(&format!(
                r#"
routes:
  - id: r
    match:
      methods: ["GET"]
      path_prefix: "/x"
      query_present: ["{name}"]
    legacy_upstream: "https://l"
    mode: legacy_only
"#
            ));
            assert!(
                errs.iter()
                    .any(|e| e.location.contains("match.query_present")
                        && e.message.contains(expected)),
                "expected {expected:?} for {name:?} in {errs:?}"
            );
        }
    }

    #[test]
    fn a_name_in_both_query_fields_is_caught() {
        let errs = errors(
            r#"
routes:
  - id: r
    match:
      methods: ["GET"]
      path_prefix: "/x"
      query_present: ["prompt"]
      query_absent: ["prompt"]
    legacy_upstream: "https://l"
    mode: legacy_only
"#,
        );
        assert!(errs.iter().any(|e| e.location.contains("\"r\"")
            && e.location.contains("match.query_present")
            && e.message.contains("could never match")));
    }

    /// Not provably disjoint: a request carrying both `a` and `b` satisfies each
    /// route, so which one wins would be config-order luck.
    #[test]
    fn two_satisfiable_query_present_routes_on_one_prefix_are_rejected() {
        let errs = errors(
            r#"
routes:
  - id: first
    match: { methods: ["GET"], path_prefix: "/x", query_present: ["a"] }
    legacy_upstream: "https://l"
    mode: legacy_only
  - id: second
    match: { methods: ["GET", "HEAD"], path_prefix: "/x", query_present: ["b"] }
    legacy_upstream: "https://l"
    mode: legacy_only
"#,
        );
        let overlap: Vec<&ValidationError> = errs
            .iter()
            .filter(|e| e.message.contains("provably disjoint"))
            .collect();
        assert_eq!(overlap.len(), 1, "{errs:?}");
        assert!(overlap[0].location.contains("\"second\""));
        assert!(overlap[0].message.contains("\"first\""));
    }

    /// `query_present: [a]` vs `query_absent: [b]` over unrelated names is also
    /// rejected: a request with `a` and without `b` satisfies both.
    #[test]
    fn unrelated_present_and_absent_names_on_one_prefix_are_rejected() {
        let errs = errors(
            r#"
routes:
  - id: first
    match: { methods: ["GET"], path_prefix: "/x", query_present: ["a"] }
    legacy_upstream: "https://l"
    mode: legacy_only
  - id: second
    match: { methods: ["GET"], path_prefix: "/x", query_absent: ["b"] }
    legacy_upstream: "https://l"
    mode: legacy_only
"#,
        );
        assert!(errs.iter().any(|e| e.message.contains("provably disjoint")));
    }

    /// The check only fires where two conditioned routes really compete: a
    /// different prefix (longest prefix decides), disjoint methods, or an
    /// unconditioned counterpart (the conditioned route simply wins the tie).
    #[test]
    fn query_conditioned_routes_that_cannot_compete_are_accepted() {
        for yaml in [
            r#"
routes:
  - id: first
    match: { methods: ["GET"], path_prefix: "/x/a", query_present: ["a"] }
    legacy_upstream: "https://l"
    mode: legacy_only
  - id: second
    match: { methods: ["GET"], path_prefix: "/x", query_present: ["b"] }
    legacy_upstream: "https://l"
    mode: legacy_only
"#,
            r#"
routes:
  - id: first
    match: { methods: ["GET"], path_prefix: "/x", query_present: ["a"] }
    legacy_upstream: "https://l"
    mode: legacy_only
  - id: second
    match: { methods: ["POST"], path_prefix: "/x", query_present: ["b"] }
    legacy_upstream: "https://l"
    mode: legacy_only
"#,
            r#"
routes:
  - id: first
    match: { methods: ["GET"], path_prefix: "/x", query_present: ["a"] }
    legacy_upstream: "https://l"
    mode: legacy_only
  - id: second
    match: { methods: ["GET"], path_prefix: "/x" }
    legacy_upstream: "https://l"
    mode: legacy_only
"#,
        ] {
            assert!(validate(&parse(yaml), &base()).is_ok(), "{yaml}");
        }
    }

    // -----------------------------------------------------------------------
    // Path templates (spec §5.2)
    // -----------------------------------------------------------------------

    /// Two routes carrying the given inline match blocks. Both legacy-only, so
    /// nothing but the match can produce an error.
    fn two_routes(a: &str, b: &str) -> String {
        format!(
            r#"
routes:
  - id: first
    match: {a}
    legacy_upstream: "https://l"
    mode: legacy_only
  - id: second
    match: {b}
    legacy_upstream: "https://l"
    mode: legacy_only
"#
        )
    }

    fn assert_accepted(yaml: &str) {
        assert!(validate(&parse(yaml), &base()).is_ok(), "{yaml}");
    }

    /// The pair is refused and the message names *both* routes — an operator
    /// cannot fix an ambiguity while looking at one half of it.
    fn assert_pair_refused(yaml: &str) {
        let errs = validate(&parse(yaml), &base()).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.location.contains("second") && e.message.contains("\"first\"")),
            "{yaml}\n{errs:?}"
        );
    }

    #[test]
    fn a_match_names_its_paths_exactly_one_way() {
        for m in [
            r#"{ methods: ["GET"], path_prefix: "/a", path_template: "/a/{x}" }"#,
            r#"{ methods: ["GET"] }"#,
        ] {
            let errs = errors(&two_routes(
                m,
                r#"{ methods: ["POST"], path_prefix: "/zzz" }"#,
            ));
            assert!(
                errs.iter().any(|e| e.location.ends_with(".match")
                    && e.message.contains("path_prefix")
                    && e.message.contains("path_template")),
                "{m}: {errs:?}"
            );
        }
    }

    /// Every syntax rule surfaces as a load-time error on the field that broke
    /// it, rather than as a route that quietly matches nothing.
    #[test]
    fn an_unparseable_path_template_is_caught() {
        for template in [
            "/{a}",         // no literal segment
            "/{a}/{b}",     // …still none
            "/",            // the bare root
            "/a/",          // trailing slash
            "/a//b",        // empty segment
            "/v{n}/x",      // parameter that does not span its segment
            "/a/{id}/{id}", // duplicate name
            "/a/{1id}",     // not an identifier
            "a/{id}",       // no leading slash
        ] {
            let yaml = two_routes(
                &format!(r#"{{ methods: ["GET"], path_template: "{template}" }}"#),
                r#"{ methods: ["POST"], path_prefix: "/zzz" }"#,
            );
            let errs = errors(&yaml);
            assert!(
                errs.iter().any(|e| e.location.contains("path_template")),
                "{template:?} should be rejected: {errs:?}"
            );
        }
    }

    /// A template that is strictly narrower than another is legal: the matcher
    /// consults the narrower first, so there is exactly one answer.
    #[test]
    fn a_strictly_narrower_template_is_accepted() {
        assert_accepted(&two_routes(
            r#"{ methods: ["GET"], path_template: "/conversations/{id}" }"#,
            r#"{ methods: ["GET"], path_template: "/conversations/export" }"#,
        ));
        // Different segment counts cannot both match one path.
        assert_accepted(&two_routes(
            r#"{ methods: ["GET"], path_template: "/a/{x}" }"#,
            r#"{ methods: ["GET"], path_template: "/a/{x}/{y}" }"#,
        ));
        // Clashing literals in the same position.
        assert_accepted(&two_routes(
            r#"{ methods: ["GET"], path_template: "/a/b/{x}" }"#,
            r#"{ methods: ["GET"], path_template: "/a/c/{x}" }"#,
        ));
    }

    /// Falsification (codex review, C1): a narrower UNCONDITIONED template
    /// must not steal requests from a broader QUERY-CONDITIONED one. The
    /// narrow shape wins the sort on every path it matches, so without this
    /// rule `/oauth2/auth?login_verifier=x` would route to the narrow route
    /// even though the broad route's condition was written to capture it —
    /// the within-tier twin of the conditioned-prefix protection.
    #[test]
    fn a_narrower_template_cannot_steal_a_broader_conditioned_one() {
        assert_pair_refused(&two_routes(
            r#"{ methods: ["POST"], path_template: "/oauth2/{action}", query_present: ["login_verifier"] }"#,
            r#"{ methods: ["POST"], path_template: "/oauth2/auth" }"#,
        ));
        // Orientation-independent: the narrow route listed first refuses too.
        assert_pair_refused(&two_routes(
            r#"{ methods: ["POST"], path_template: "/oauth2/auth" }"#,
            r#"{ methods: ["POST"], path_template: "/oauth2/{action}", query_present: ["login_verifier"] }"#,
        ));
        // Control: provable query-disjointness makes the same pair legal —
        // the narrow route can no longer receive the condition's requests.
        assert_accepted(&two_routes(
            r#"{ methods: ["POST"], path_template: "/oauth2/{action}", query_present: ["login_verifier"] }"#,
            r#"{ methods: ["POST"], path_template: "/oauth2/auth", query_absent: ["login_verifier"] }"#,
        ));
        // Control: the reverse orientation — narrower CONDITIONED over a
        // broader unconditioned fallback — is ordinary refinement.
        assert_accepted(&two_routes(
            r#"{ methods: ["POST"], path_template: "/oauth2/{action}" }"#,
            r#"{ methods: ["POST"], path_template: "/oauth2/auth", query_present: ["login_verifier"] }"#,
        ));
    }

    /// The other half of the steal rule, pinned because it is the case the
    /// orientation check is easiest to over-refuse: BOTH templates conditioned,
    /// overlapping (not provably disjoint), narrower subsuming into broader.
    /// Path specificity orders the pair — the narrower wins where it matches —
    /// and the condition on the narrower shape is the operator declaring which
    /// of *its own* requests it wants, so this is ordinary refinement, not a
    /// steal. Only equal-rank pairs (identical templates, equal prefixes) need
    /// provable disjointness (spec Section 5.2, Precedence).
    #[test]
    fn a_conditioned_narrower_template_refines_a_conditioned_broader_one() {
        assert_accepted(&two_routes(
            r#"{ methods: ["POST"], path_template: "/oauth2/{action}", query_present: ["login_verifier"] }"#,
            r#"{ methods: ["POST"], path_template: "/oauth2/auth", query_present: ["login_verifier"] }"#,
        ));
        // Orientation-independent: the narrow route listed first is the same
        // refinement, accepted the same way.
        assert_accepted(&two_routes(
            r#"{ methods: ["POST"], path_template: "/oauth2/auth", query_present: ["login_verifier"] }"#,
            r#"{ methods: ["POST"], path_template: "/oauth2/{action}", query_present: ["login_verifier"] }"#,
        ));
        // Unrelated, non-disjoint conditions (a `query_present` pair over
        // different names, which the equal-shape rule would refuse) are also
        // fine once the shapes are subsumption-ordered.
        assert_accepted(&two_routes(
            r#"{ methods: ["GET"], path_template: "/conversations/{id}", query_present: ["a"] }"#,
            r#"{ methods: ["GET"], path_template: "/conversations/export", query_absent: ["b"] }"#,
        ));
    }

    /// Co-matchable but incomparable: each template pins a segment the other
    /// leaves open, so no order is defensible and Limen refuses to start.
    #[test]
    fn incomparable_templates_are_refused() {
        for (a, b) in [("/a/{x}/c", "/a/b/{y}"), ("/a/{x}/{y}", "/{t}/b/c")] {
            // The `query_present` on the second route shows that query
            // conditions do not rescue this shape: the ambiguity is in the
            // path, and the matcher orders on the path first.
            assert_pair_refused(&two_routes(
                &format!(r#"{{ methods: ["GET"], path_template: "{a}" }}"#),
                &format!(r#"{{ methods: ["GET"], path_template: "{b}", query_present: ["v"] }}"#),
            ));
        }
    }

    /// Identical shapes fall back to the equal-prefix rules verbatim.
    #[test]
    fn identical_templates_follow_the_equal_prefix_rules() {
        // Exactly one conditioned: the conditioned route wins, as at an equal
        // prefix. Parameter names are not part of the shape.
        assert_accepted(&two_routes(
            r#"{ methods: ["GET"], path_template: "/oauth2/{action}" }"#,
            r#"{ methods: ["GET"], path_template: "/oauth2/{verb}", query_present: ["v"] }"#,
        ));
        // Both conditioned and provably disjoint.
        assert_accepted(&two_routes(
            r#"{ methods: ["GET"], path_template: "/oauth2/{action}", query_present: ["v"] }"#,
            r#"{ methods: ["GET"], path_template: "/oauth2/{action}", query_absent: ["v"] }"#,
        ));
        // Both conditioned, not provably disjoint.
        assert_pair_refused(&two_routes(
            r#"{ methods: ["GET"], path_template: "/oauth2/{action}", query_present: ["a"] }"#,
            r#"{ methods: ["GET"], path_template: "/oauth2/{action}", query_present: ["b"] }"#,
        ));
        // Neither conditioned: nothing distinguishes them at all.
        assert_pair_refused(&two_routes(
            r#"{ methods: ["GET"], path_template: "/oauth2/{action}" }"#,
            r#"{ methods: ["GET"], path_template: "/oauth2/{verb}" }"#,
        ));
    }

    /// Disjoint methods make any path overlap moot.
    #[test]
    fn overlapping_paths_on_disjoint_methods_are_accepted() {
        assert_accepted(&two_routes(
            r#"{ methods: ["GET"], path_template: "/a/{x}/c" }"#,
            r#"{ methods: ["POST"], path_template: "/a/b/{y}" }"#,
        ));
        assert_accepted(&two_routes(
            r#"{ methods: ["GET"], path_template: "/conversations/{id}" }"#,
            r#"{ methods: ["POST"], path_prefix: "/conversations/export" }"#,
        ));
    }

    /// A template that lies entirely under a prefix route refines it — the case
    /// templates exist for — and is accepted from either config position.
    #[test]
    fn a_template_contained_in_a_prefix_route_is_accepted() {
        for (a, b) in [
            (
                r#"{ methods: ["GET"], path_template: "/conversations/{id}" }"#,
                r#"{ methods: ["GET"], path_prefix: "/conversations/" }"#,
            ),
            (
                r#"{ methods: ["GET"], path_prefix: "/" }"#,
                r#"{ methods: ["GET"], path_template: "/conversations/{id}" }"#,
            ),
            // A prefix ending mid-segment still contains every path the
            // template matches.
            (
                r#"{ methods: ["GET"], path_prefix: "/conv" }"#,
                r#"{ methods: ["GET"], path_template: "/conversations/{id}" }"#,
            ),
            // …and one that cannot meet it at all.
            (
                r#"{ methods: ["GET"], path_prefix: "/voices/" }"#,
                r#"{ methods: ["GET"], path_template: "/conversations/{id}" }"#,
            ),
        ] {
            assert_accepted(&two_routes(a, b));
        }
    }

    /// The prefix is longer than the template's literal head: the template
    /// would take some of that route's traffic and leave the rest, on a
    /// boundary neither route states.
    #[test]
    fn a_template_that_half_covers_a_prefix_route_is_refused() {
        let yaml = two_routes(
            r#"{ methods: ["GET"], path_template: "/conversations/{id}" }"#,
            r#"{ methods: ["GET"], path_prefix: "/conversations/export" }"#,
        );
        assert_pair_refused(&yaml);
        let errs = errors(&yaml);
        // The message says what to do about it.
        assert!(
            errs.iter()
                .any(|e| e.message.contains("all-literal path_template")),
            "{errs:?}"
        );
    }

    /// The verifier shape, verbatim: an unconditioned template over a
    /// conditioned safety route must refuse to start, and the complementary
    /// `query_absent` is what makes it legal.
    #[test]
    fn a_template_over_a_conditioned_prefix_route_needs_a_complementary_condition() {
        let verifier = r#"{ methods: ["GET"], path_prefix: "/oauth2/auth", query_present: ["login_verifier"] }"#;
        assert_pair_refused(&two_routes(
            verifier,
            r#"{ methods: ["GET"], path_template: "/oauth2/{action}" }"#,
        ));
        assert_accepted(&two_routes(
            verifier,
            r#"{ methods: ["GET"], path_template: "/oauth2/{action}", query_absent: ["login_verifier"] }"#,
        ));
    }

    #[test]
    fn out_of_range_budget_is_caught() {
        let errs = errors(
            r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: shadow_legacy_primary
    budget:
      max_new_p95_latency_ratio: -1.0
      max_new_error_rate_ratio: 0
      max_mismatch_rate: 2.0
"#,
        );
        assert!(locations(&errs)
            .iter()
            .any(|l| l.contains("max_new_p95_latency_ratio")));
        assert!(locations(&errs)
            .iter()
            .any(|l| l.contains("max_mismatch_rate")));
    }

    #[test]
    fn redis_provider_validates_url() {
        let errs = errors(
            r#"
flags:
  provider: redis
  redis: { url: "http://not-redis", key_prefix: "p", refresh_interval_ms: 1000 }
  stale_ttl_ms: 30000
  fail_safe_mode: legacy_only
"#,
        );
        assert!(locations(&errs).contains(&"flags.redis.url"));
    }

    #[test]
    fn diff_sink_dir_must_be_non_empty_but_need_not_exist() {
        // A directory that does not exist (nor its parent) is fine — the sink
        // creates it on the first mismatch.
        let ok = parse("diff_sink:\n  dir: \"/nonexistent-parent/limen-diffs\"\n");
        assert_eq!(
            ok.diff_sink.as_ref().unwrap().dir,
            PathBuf::from("/nonexistent-parent/limen-diffs")
        );
        assert!(validate(&ok, &base()).is_ok());

        let errs = errors("diff_sink:\n  dir: \"\"\n");
        assert!(locations(&errs).contains(&"diff_sink.dir"));
    }

    #[test]
    fn bad_listen_addr_is_caught() {
        let errs = errors("server:\n  listen_addr: \"not-an-addr\"\n  graceful_shutdown_timeout_ms: 1\n  request_body_limit_bytes: 1\n");
        assert!(locations(&errs).contains(&"server.listen_addr"));
    }

    #[test]
    fn empty_inline_json_block_conflicts_with_contract() {
        // `comparison: { json: {} }` alongside a contract is flagged: the block
        // is present even though it carries no rules (spec §4.4).
        let errs = errors(
            r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: shadow_legacy_primary
    contract: "./c.contract.yaml#r"
    comparison:
      enabled: true
      sample_rate: 1.0
      max_body_bytes: 1024
      json: {}
"#,
        );
        assert!(errs.iter().any(|e| e.message.contains("not both")));
    }

    #[test]
    fn referenced_contract_with_bad_version_is_flagged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("svc.contract.yaml"),
            "version: 2\nservice: s\nroutes:\n  - id: get\n    match: { methods: [GET], path_template: \"/x\" }\n",
        )
        .unwrap();
        let yaml = r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: shadow_legacy_primary
    contract: "./svc.contract.yaml#get"
"#;
        let errs = validate(&parse(yaml), dir.path()).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| e.message.contains("unsupported contract version 2")));
    }

    #[test]
    fn referenced_contract_semantics_reported_once_not_per_route() {
        // Two routes referencing the same bad-version contract should yield the
        // version error exactly once (validated per distinct contract file).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("svc.contract.yaml"),
            "version: 9\nservice: s\nroutes:\n  - id: a\n    match: { methods: [GET], path_template: \"/a\" }\n  - id: b\n    match: { methods: [GET], path_template: \"/b\" }\n",
        )
        .unwrap();
        let yaml = r#"
routes:
  - id: r1
    match: { methods: ["GET"], path_prefix: "/a" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: shadow_legacy_primary
    contract: "./svc.contract.yaml#a"
  - id: r2
    match: { methods: ["GET"], path_prefix: "/b" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: shadow_legacy_primary
    contract: "./svc.contract.yaml#b"
"#;
        let errs = validate(&parse(yaml), dir.path()).unwrap_err();
        let version_errors = errs
            .iter()
            .filter(|e| e.message.contains("unsupported contract version"))
            .count();
        assert_eq!(version_errors, 1);
    }

    #[test]
    fn contract_reference_resolution_against_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("svc.contract.yaml"),
            "version: 1\nservice: s\ndefaults:\n  json:\n    ignore_paths: [\"$.ok\"]\nroutes:\n  - id: get\n    match: { methods: [GET], path_template: \"/x\" }\n",
        )
        .unwrap();

        let good = r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: shadow_legacy_primary
    contract: "./svc.contract.yaml#get"
"#;
        assert!(validate(&parse(good), dir.path()).is_ok());

        // Missing route fragment in the contract.
        let bad = good.replace("#get", "#missing");
        let errs = validate(&parse(&bad), dir.path()).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("no route")));
    }

    #[test]
    fn valid_observe_block_passes() {
        assert!(validate(&parse("observe: {}"), &base()).is_ok());
        assert!(validate(
            &parse("observe:\n  sample_rate: 0.5\n  max_query_names: 8\n"),
            &base()
        )
        .is_ok());
    }

    #[test]
    fn observe_sample_rate_out_of_range_is_caught() {
        for rate in ["-0.1", "1.5"] {
            let errs = errors(&format!("observe:\n  sample_rate: {rate}\n"));
            assert!(locations(&errs).contains(&"observe.sample_rate"));
        }
    }

    #[test]
    fn observe_zero_bounds_are_caught() {
        // All three accumulate: a config with three mistakes reports three.
        let errs = errors(
            r#"
observe:
  max_query_names: 0
  max_path_shapes: 0
  max_fingerprints: 0
"#,
        );
        for field in [
            "observe.max_query_names",
            "observe.max_path_shapes",
            "observe.max_fingerprints",
        ] {
            assert!(locations(&errs).contains(&field), "{field}");
        }
    }

    #[test]
    fn observe_bounds_above_the_ceiling_are_caught() {
        // A cap the operator can raise without limit is not a cap: invariant 6
        // requires these traffic-keyed maps to be bounded, so the config may not
        // remove the bound.
        let errs = errors(&format!(
            "observe:\n  max_query_names: {}\n  max_path_shapes: 999999999\n  max_fingerprints: \
             {}\n",
            MAX_OBSERVE_BOUND + 1,
            MAX_OBSERVE_BOUND + 1
        ));
        for field in [
            "observe.max_query_names",
            "observe.max_path_shapes",
            "observe.max_fingerprints",
        ] {
            assert!(locations(&errs).contains(&field), "{field}");
        }
        // The ceiling itself is legal — the rejection starts one past it.
        assert!(validate(
            &parse(&format!(
                "observe:\n  max_query_names: {MAX_OBSERVE_BOUND}\n"
            )),
            &base()
        )
        .is_ok());
    }

    #[test]
    fn observe_profile_path_cannot_also_serve_metrics() {
        // Without this the duplicate route panics inside axum's router build.
        let errs = errors("observe: {}\nmetrics:\n  path: \"/observe/profile\"\n");
        assert!(errs
            .iter()
            .any(|e| e.location == "metrics.path" && e.message.contains("/observe/profile")));
        // The same metrics path is fine with observation off — nothing else
        // claims the route.
        assert!(validate(&parse("metrics:\n  path: \"/observe/profile\"\n"), &base()).is_ok());
    }

    #[test]
    fn metrics_path_cannot_collide_with_a_fixed_control_plane_route() {
        // Unconditional, unlike the observe/profile check above: these three
        // routes are always registered, so the collision must be caught with
        // no observe block in play. Iterating the shared constant (rather than
        // hardcoding the three literals) is what proves validation actually
        // tracks the router instead of just agreeing with it today.
        for path in CONTROL_PLANE_RESERVED_PATHS {
            let errs = errors(&format!("metrics:\n  path: {path:?}\n"));
            assert!(
                errs.iter()
                    .any(|e| e.location == "metrics.path" && e.message.contains(path)),
                "{path}: {errs:?}"
            );
        }
    }

    #[test]
    fn an_ordinary_metrics_path_validates_clean() {
        assert!(validate(&parse("metrics:\n  path: \"/metrics\"\n"), &base()).is_ok());
    }
}
