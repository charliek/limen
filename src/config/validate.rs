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
    RolloutConfig, RouteConfig, RouteMode, TimeoutsConfig, UpstreamTlsConfig,
};
use crate::contract::load as contract_load;
use crate::contract::model::{BehavioralRules, Contract};

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

    let mut seen_ids: HashSet<&str> = HashSet::new();
    let mut contracts = ContractCache::default();
    for (i, route) in config.routes.iter().enumerate() {
        validate_route(i, route, base_dir, &mut seen_ids, &mut contracts, &mut errs);
    }

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

#[allow(clippy::too_many_arguments)]
fn validate_route<'a>(
    index: usize,
    route: &'a RouteConfig,
    base_dir: &Path,
    seen_ids: &mut HashSet<&'a str>,
    contracts: &mut ContractCache,
    errs: &mut Errors,
) {
    let loc = |field: &str| format!("routes[{index}] {:?}.{field}", route.id);

    if route.id.trim().is_empty() {
        errs.push(format!("routes[{index}].id"), "must not be empty");
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
    let prefix = &route.r#match.path_prefix;
    if prefix.is_empty() {
        errs.push(loc("match.path_prefix"), "must not be empty");
    } else if !prefix.starts_with('/') {
        errs.push(
            loc("match.path_prefix"),
            format!("must start with '/' (got {prefix:?})"),
        );
    }
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
}
