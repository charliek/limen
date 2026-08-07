//! Serde model for `limen.config.yaml` (spec §5.2).
//!
//! These structs describe the *operational* configuration — listeners, flag
//! provider, and per-route routing/rollout/timeout/breaker policy. The
//! *behavioral* comparison vocabulary (what to compare and how) is owned by the
//! [contract][crate::contract]; a route either references a contract or inlines
//! the same vocabulary, never both (enforced in [`super::validate`]).
//!
//! Every field has a built-in default (spec §5.1), so a minimal config file
//! parses and the missing fields fall back to safe values.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::contract::model::{BehavioralRules, JsonRules, LocationRules, SetCookieRules};
use crate::flags::FlagValue;

/// The top-level configuration document.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Data-plane listener and request limits.
    #[serde(default)]
    pub server: ServerConfig,
    /// Control-plane (metrics + health) listener.
    #[serde(default)]
    pub metrics: MetricsConfig,
    /// TLS settings for upstream calls.
    #[serde(default)]
    pub upstream_tls: UpstreamTlsConfig,
    /// Feature-flag provider and fail-safe policy.
    #[serde(default)]
    pub flags: FlagsConfig,
    /// Optional durable sink for comparison mismatches (spec §10.4). Absent =
    /// mismatches are counted and logged only.
    #[serde(default)]
    pub diff_sink: Option<DiffSinkConfig>,
    /// The routing table.
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
    /// Debug-only switches. Absent (the normal case) = every debug affordance
    /// is off; see [`DebugConfig`].
    #[serde(default)]
    pub debug: Option<DebugConfig>,
}

impl Config {
    /// Whether the debug sink canary is enabled (absent block = off).
    pub fn sink_canary_enabled(&self) -> bool {
        self.debug.is_some_and(|d| d.sink_canary)
    }
}

/// Debug-only switches, off unless the block is present and says otherwise.
///
/// These exist for proving the pipeline bites (`limen verdict --canary`), not
/// for production operation — `limen run` logs a loud warning at startup when
/// any of them is on. Deliberately config-gated rather than
/// `cfg(debug_assertions)`- or feature-gated: campaign runners build limen
/// `--release`, which is exactly where the proof has to work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DebugConfig {
    /// Expose `POST /debug/canary` on the control plane, which injects one
    /// synthetic mismatch through the real compare → observer → sink pipeline
    /// under the reserved route id `__limen_canary__`.
    pub sink_canary: bool,
}

/// The durable mismatch sink (spec §10.4).
///
/// Declaring the block turns the sink on; there is nothing to enable
/// separately, and no retention policy — files rotate by UTC date and are left
/// for the operator's existing log-retention tooling to prune.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiffSinkConfig {
    /// Directory the daily `mismatches-<YYYY-MM-DD>.jsonl` files are written
    /// to. Relative paths resolve against the process working directory (like
    /// `flags.file.path`), and the directory is created on the first mismatch.
    pub dir: PathBuf,
}

/// Data-plane listener configuration.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Address the data plane binds (e.g. `0.0.0.0:8080`).
    pub listen_addr: String,
    /// How long to drain in-flight requests on shutdown.
    pub graceful_shutdown_timeout_ms: u64,
    /// Hard cap on buffered request bodies.
    pub request_body_limit_bytes: u64,
    /// Maximum concurrent in-flight shadow requests across all routes; excess
    /// shadows are skipped rather than queued (spec §9.3). `0` means no limit.
    #[serde(default = "default_shadow_concurrency_limit")]
    pub shadow_concurrency_limit: usize,
}

fn default_shadow_concurrency_limit() -> usize {
    100
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:8080".to_string(),
            graceful_shutdown_timeout_ms: 10_000,
            request_body_limit_bytes: 1_048_576,
            shadow_concurrency_limit: default_shadow_concurrency_limit(),
        }
    }
}

/// Control-plane listener configuration.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Address the control plane binds (e.g. `0.0.0.0:9090`).
    pub listen_addr: String,
    /// Path the Prometheus exposition is served on.
    pub path: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:9090".to_string(),
            path: "/metrics".to_string(),
        }
    }
}

/// TLS settings applied to the upstream client (spec §5.2, §11.4).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpstreamTlsConfig {
    /// Verify upstream certificates (on by default).
    pub verify_certificates: bool,
    /// Optional custom CA bundle for internal PKI.
    #[serde(default)]
    pub ca_bundle_path: Option<PathBuf>,
}

impl Default for UpstreamTlsConfig {
    fn default() -> Self {
        Self {
            verify_certificates: true,
            ca_bundle_path: None,
        }
    }
}

/// Which flag provider to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagProviderKind {
    /// Values fixed in config; never stale.
    Static,
    /// Polled from a YAML file.
    File,
    /// Polled from a Redis key space.
    Redis,
}

/// Behavior to fall back to when flags are stale or unavailable (spec §8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailSafeMode {
    /// Route all traffic to legacy — the safe default.
    LegacyOnly,
}

/// Feature-flag configuration.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FlagsConfig {
    /// The active provider.
    pub provider: FlagProviderKind,
    /// Static-provider values (used when `provider = static`).
    #[serde(default, rename = "static")]
    pub static_values: StaticFlagsConfig,
    /// File-provider settings.
    #[serde(default)]
    pub file: FileFlagsConfig,
    /// Redis-provider settings.
    #[serde(default)]
    pub redis: RedisFlagsConfig,
    /// After this staleness, apply `fail_safe_mode`.
    pub stale_ttl_ms: u64,
    /// What to do when flags are stale/unavailable.
    pub fail_safe_mode: FailSafeMode,
}

impl Default for FlagsConfig {
    fn default() -> Self {
        Self {
            provider: FlagProviderKind::Static,
            static_values: StaticFlagsConfig::default(),
            file: FileFlagsConfig::default(),
            redis: RedisFlagsConfig::default(),
            stale_ttl_ms: 30_000,
            fail_safe_mode: FailSafeMode::LegacyOnly,
        }
    }
}

/// Static-provider values.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StaticFlagsConfig {
    /// Flag key/value pairs.
    #[serde(default)]
    pub values: BTreeMap<String, FlagValue>,
}

/// File-provider settings.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileFlagsConfig {
    /// Path to the YAML flags file.
    pub path: PathBuf,
    /// Poll interval.
    pub refresh_interval_ms: u64,
}

impl Default for FileFlagsConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("./flags.local.yaml"),
            refresh_interval_ms: 1_000,
        }
    }
}

/// Redis-provider settings.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RedisFlagsConfig {
    /// Redis connection URL (`redis://…` or `rediss://…`).
    pub url: String,
    /// Key prefix under which flags live.
    pub key_prefix: String,
    /// Poll interval.
    pub refresh_interval_ms: u64,
}

impl Default for RedisFlagsConfig {
    fn default() -> Self {
        Self {
            url: "redis://localhost:6379".to_string(),
            key_prefix: "limen:flags:".to_string(),
            refresh_interval_ms: 1_000,
        }
    }
}

/// The five route modes (spec §6). Each route declares exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteMode {
    /// Legacy serves everything; no new traffic.
    LegacyOnly,
    /// New serves everything.
    NewOnly,
    /// Legacy serves the client; eligible reads are shadowed to new.
    ShadowLegacyPrimary,
    /// Deterministic per-key split between legacy and new by percentage.
    PercentageSplit,
    /// New is primary; fall back to legacy on failure.
    FailoverToLegacy,
}

impl RouteMode {
    /// Whether this mode ever sends traffic to the legacy upstream.
    pub fn uses_legacy(self) -> bool {
        !matches!(self, RouteMode::NewOnly)
    }

    /// Whether this mode ever sends traffic to the new upstream.
    pub fn uses_new(self) -> bool {
        !matches!(self, RouteMode::LegacyOnly)
    }

    /// The serialized (snake_case) name, for display and error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            RouteMode::LegacyOnly => "legacy_only",
            RouteMode::NewOnly => "new_only",
            RouteMode::ShadowLegacyPrimary => "shadow_legacy_primary",
            RouteMode::PercentageSplit => "percentage_split",
            RouteMode::FailoverToLegacy => "failover_to_legacy",
        }
    }
}

/// A single route.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    /// Stable unique identifier (also a metric label).
    pub id: String,
    /// What requests this route matches.
    pub r#match: RouteMatch,
    /// Legacy upstream base URL (required unless mode is `new_only`).
    #[serde(default)]
    pub legacy_upstream: Option<String>,
    /// New upstream base URL (required unless mode is `legacy_only`).
    #[serde(default)]
    pub new_upstream: Option<String>,
    /// The route mode.
    pub mode: RouteMode,
    /// Optional `path#routeId` contract reference for behavioral rules.
    #[serde(default)]
    pub contract: Option<String>,
    /// May this route auto-fail-over a failed in-flight request to legacy?
    /// Required to be `true` for `failover_to_legacy` with non-idempotent
    /// methods (spec §5.3, §6.5).
    #[serde(default)]
    pub failover_safe: bool,
    /// Rollout settings (required for `percentage_split`).
    #[serde(default)]
    pub rollout: Option<RolloutConfig>,
    /// Per-route timeouts.
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
    /// Comparison policy (operational gate + optional inline behavioral rules).
    #[serde(default)]
    pub comparison: ComparisonConfig,
    /// Circuit-breaker settings.
    #[serde(default)]
    pub circuit_breaker: CircuitBreakerConfig,
    /// Forward-looking rollout budget (documented, not enforced in MVP §12.1).
    #[serde(default)]
    pub budget: Option<BudgetConfig>,
}

/// Match predicate: method + path prefix, optionally narrowed by which query
/// parameters the request carries (longest prefix wins; spec §5.2).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteMatch {
    /// HTTP methods this route matches.
    pub methods: Vec<String>,
    /// Path prefix this route matches.
    pub path_prefix: String,
    /// Query parameter names that must **all** be present for this route to
    /// match (presence only — values are irrelevant). Empty (the default) =
    /// unconditioned. Exists so a path whose hops are not equally safe to
    /// shadow can be split into a conditioned route that only relays and an
    /// unconditioned one that stays compared (spec §5.2).
    #[serde(default)]
    pub query_present: Vec<String>,
    /// Query parameter names of which **none** may be present for this route to
    /// match. Empty (the default) = unconditioned. A name may not appear in both
    /// `query_present` and `query_absent` (see [`super::validate`]).
    #[serde(default)]
    pub query_absent: Vec<String>,
}

impl RouteMatch {
    /// Whether this match declares any query condition. A route that does not
    /// behaves byte-identically to one written before the fields existed; a
    /// route that does wins the tiebreak against an unconditioned route on the
    /// same prefix (spec §5.2).
    pub fn is_query_conditioned(&self) -> bool {
        !self.query_present.is_empty() || !self.query_absent.is_empty()
    }
}

/// Rollout configuration for `percentage_split` (spec §5.2, §6.4).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RolloutConfig {
    /// Flag key holding the rollout percentage (0–100).
    pub percentage_flag: String,
    /// Percentage to use when the flag is unset.
    #[serde(default)]
    pub default_percentage: f64,
    /// How to derive the deterministic assignment key.
    pub assignment_key: AssignmentKey,
}

/// How a request's deterministic assignment key is derived.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssignmentKey {
    /// Header whose value is the assignment key.
    #[serde(default)]
    pub header: Option<String>,
    /// Fallback when the header is absent.
    pub fallback: AssignmentFallback,
}

/// Fallback strategy when the assignment key is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentFallback {
    /// Assign randomly per request (MVP).
    RequestRandom,
}

/// Per-route timeouts.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimeoutsConfig {
    /// Timeout for the primary upstream call.
    pub primary_ms: u64,
    /// Timeout for the shadow upstream call (never on the client path).
    pub shadow_ms: u64,
}

impl Default for TimeoutsConfig {
    fn default() -> Self {
        Self {
            primary_ms: 2_000,
            shadow_ms: 2_000,
        }
    }
}

/// Comparison policy: the operational gate plus optional inline behavioral
/// rules (the inline-rules fallback when no contract is referenced; spec §4.4).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ComparisonConfig {
    /// Operational gate: is comparison enabled at all for this route?
    pub enabled: bool,
    /// Fraction of eligible requests to buffer and compare (0–1).
    pub sample_rate: f64,
    /// Skip comparison above this body size.
    pub max_body_bytes: u64,
    /// Floor asserted by `limen verdict`: the minimum number of comparisons
    /// this route must have recorded for a campaign verdict to count it as
    /// exercised (spec §12.1's operator-checked-gate territory). `0` opts the
    /// route out of the floor explicitly. Ignored by the proxy itself — this
    /// is a verdict-time expectation, not a runtime behavior knob.
    #[serde(default = "default_min_comparisons")]
    pub min_comparisons: u64,
    /// Write methods this route opts into shadowing (spec §6.1). Empty (the
    /// default) keeps safety invariant 3 intact: only `GET`/`HEAD` reads are
    /// shadowed. Only `POST` may be listed today, and only on a
    /// `shadow_legacy_primary` route with `enabled: true` (see
    /// [`super::validate`]); an opted-in request's body is buffered once,
    /// bounded by `max_body_bytes`, and replayed byte-identically to both
    /// upstreams.
    #[serde(default)]
    pub shadow_methods: Vec<String>,
    /// Inline: compare HTTP status (behavioral; conflicts with `contract`).
    #[serde(default)]
    pub compare_status: Option<bool>,
    /// Inline: compare normalized body (behavioral; conflicts with `contract`).
    #[serde(default)]
    pub compare_body: Option<bool>,
    /// Inline: header names to compare (behavioral; conflicts with `contract`).
    #[serde(default)]
    pub compare_headers: Option<Vec<String>>,
    /// Inline: JSON normalization rules (behavioral; conflicts with `contract`).
    #[serde(default)]
    pub json: Option<JsonRules>,
    /// Inline: `Set-Cookie` comparison (behavioral; conflicts with `contract`).
    #[serde(default)]
    pub set_cookie: Option<SetCookieRules>,
    /// Inline: `Location` comparison (behavioral; conflicts with `contract`).
    #[serde(default)]
    pub location: Option<LocationRules>,
}

fn default_min_comparisons() -> u64 {
    1
}

impl Default for ComparisonConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sample_rate: 0.0,
            max_body_bytes: 262_144,
            min_comparisons: default_min_comparisons(),
            shadow_methods: Vec::new(),
            compare_status: None,
            compare_body: None,
            compare_headers: None,
            json: None,
            set_cookie: None,
            location: None,
        }
    }
}

impl ComparisonConfig {
    /// Whether the route declares an inline behavioral block (which is mutually
    /// exclusive with a contract reference). A present-but-empty `json: {}`
    /// still counts: per spec §4.4 the conflict is about the *block* being
    /// present, and an empty block alongside a contract is a likely mistake
    /// worth flagging. Checks fields directly so the per-route validation/print
    /// loops don't clone the rules just to ask.
    pub fn has_inline_behavioral(&self) -> bool {
        self.compare_status.is_some()
            || self.compare_body.is_some()
            || self.compare_headers.is_some()
            || self.json.is_some()
            || self.set_cookie.is_some()
            || self.location.is_some()
    }

    /// The inline behavioral rules expressed as a mergeable layer.
    pub fn inline_behavioral(&self) -> BehavioralRules {
        BehavioralRules {
            compare_status: self.compare_status,
            compare_body: self.compare_body,
            compare_headers: self.compare_headers.clone(),
            json: self.json.clone(),
            set_cookie: self.set_cookie.clone(),
            location: self.location.clone(),
        }
    }
}

/// Per-route, per-(new-)upstream circuit breaker settings (spec §9.1).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CircuitBreakerConfig {
    /// Whether the breaker is active for this route.
    pub enabled: bool,
    /// Failure-rate threshold (0–1) that opens the breaker.
    pub failure_rate_threshold: f64,
    /// Minimum observed requests before the threshold applies.
    pub min_requests: u32,
    /// How long the breaker stays open before going half-open.
    pub open_duration_ms: u64,
    /// Trial requests allowed while half-open.
    pub half_open_max_requests: u32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            failure_rate_threshold: 0.5,
            min_requests: 20,
            open_duration_ms: 30_000,
            half_open_max_requests: 5,
        }
    }
}

/// Forward-looking rollout budget (spec §12.1). Validated for shape but not
/// enforced by the MVP.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetConfig {
    /// Ceiling on new p95 / legacy p95 latency ratio.
    pub max_new_p95_latency_ratio: f64,
    /// Ceiling on new 5xx / legacy 5xx error-rate ratio.
    pub max_new_error_rate_ratio: f64,
    /// Parity ceiling: fraction of compared requests that may mismatch (0–1).
    pub max_mismatch_rate: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config_uses_defaults() {
        let cfg: Config = serde_yaml::from_str("{}").unwrap();
        assert_eq!(cfg.server.listen_addr, "0.0.0.0:8080");
        assert_eq!(cfg.metrics.listen_addr, "0.0.0.0:9090");
        assert!(cfg.upstream_tls.verify_certificates);
        assert_eq!(cfg.flags.provider, FlagProviderKind::Static);
        assert_eq!(cfg.flags.fail_safe_mode, FailSafeMode::LegacyOnly);
        assert!(cfg.routes.is_empty());
    }

    #[test]
    fn partial_nested_block_fills_remaining_defaults() {
        // Specifying only one field of a block must not require the others —
        // built-in defaults fill the rest (spec §5.1 layering).
        let cfg: Config =
            serde_yaml::from_str("server:\n  listen_addr: \"0.0.0.0:1234\"\n").unwrap();
        assert_eq!(cfg.server.listen_addr, "0.0.0.0:1234");
        assert_eq!(cfg.server.graceful_shutdown_timeout_ms, 10_000);
        assert_eq!(cfg.server.request_body_limit_bytes, 1_048_576);
    }

    #[test]
    fn parses_a_full_route() {
        let yaml = r#"
routes:
  - id: get-device
    match:
      methods: ["GET"]
      path_prefix: "/devices/"
    legacy_upstream: "https://legacy.internal"
    new_upstream: "https://new.internal"
    mode: shadow_legacy_primary
    contract: "./c.contract.yaml#get-device"
    timeouts: { primary_ms: 1500, shadow_ms: 1500 }
    comparison: { enabled: true, sample_rate: 0.1, max_body_bytes: 262144 }
    circuit_breaker: { enabled: true, failure_rate_threshold: 0.25, min_requests: 20, open_duration_ms: 30000, half_open_max_requests: 5 }
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let r = &cfg.routes[0];
        assert_eq!(r.id, "get-device");
        assert_eq!(r.mode, RouteMode::ShadowLegacyPrimary);
        assert_eq!(r.r#match.methods, vec!["GET"]);
        assert!(r.comparison.enabled);
        assert!(!r.comparison.has_inline_behavioral());
        assert!(!r.failover_safe);
    }

    #[test]
    fn query_conditions_default_to_absent_and_parse_when_given() {
        let yaml = r#"
routes:
  - id: plain
    match: { methods: ["GET"], path_prefix: "/oauth2/auth" }
    legacy_upstream: "https://legacy.internal"
    mode: legacy_only
  - id: verifier
    match:
      methods: ["GET"]
      path_prefix: "/oauth2/auth"
      query_present: ["login_verifier"]
      query_absent: ["prompt"]
    legacy_upstream: "https://legacy.internal"
    mode: legacy_only
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        // Omitting the fields must stay indistinguishable from writing them empty.
        assert!(cfg.routes[0].r#match.query_present.is_empty());
        assert!(cfg.routes[0].r#match.query_absent.is_empty());
        assert!(!cfg.routes[0].r#match.is_query_conditioned());
        assert_eq!(cfg.routes[1].r#match.query_present, vec!["login_verifier"]);
        assert_eq!(cfg.routes[1].r#match.query_absent, vec!["prompt"]);
        assert!(cfg.routes[1].r#match.is_query_conditioned());
    }

    #[test]
    fn detects_inline_behavioral_block() {
        let yaml = r#"
routes:
  - id: r
    match: { methods: ["GET"], path_prefix: "/" }
    new_upstream: "https://new.internal"
    legacy_upstream: "https://legacy.internal"
    mode: shadow_legacy_primary
    comparison:
      enabled: true
      sample_rate: 1.0
      max_body_bytes: 1024
      json:
        ignore_paths: ["$.ts"]
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.routes[0].comparison.has_inline_behavioral());
        assert_eq!(
            cfg.routes[0]
                .comparison
                .inline_behavioral()
                .json
                .unwrap()
                .ignore_paths,
            vec!["$.ts"]
        );
    }

    #[test]
    fn unknown_top_level_key_rejected() {
        assert!(serde_yaml::from_str::<Config>("serer: {}").is_err());
    }

    #[test]
    fn debug_block_is_absent_and_off_by_default() {
        let cfg: Config = serde_yaml::from_str("{}").unwrap();
        assert!(cfg.debug.is_none());
        assert!(!cfg.sink_canary_enabled());
        // An empty block is still off: only an explicit `true` enables it.
        let cfg: Config = serde_yaml::from_str("debug: {}").unwrap();
        assert_eq!(cfg.debug, Some(DebugConfig { sink_canary: false }));
        assert!(!cfg.sink_canary_enabled());
    }

    #[test]
    fn debug_sink_canary_parses_and_rejects_typos() {
        let cfg: Config = serde_yaml::from_str("debug:\n  sink_canary: true\n").unwrap();
        assert!(cfg.sink_canary_enabled());
        // A misspelled debug switch must fail loudly rather than silently
        // leaving the canary off — a runner would then "prove" nothing.
        assert!(serde_yaml::from_str::<Config>("debug:\n  sink_canry: true\n").is_err());
    }

    #[test]
    fn static_flag_values_parse() {
        let yaml = r#"
flags:
  provider: static
  static:
    values:
      "migration.get-device.rollout_percentage": 25
      "migration.get-device.shadow_enabled": true
  stale_ttl_ms: 30000
  fail_safe_mode: legacy_only
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            cfg.flags.static_values.values["migration.get-device.rollout_percentage"].as_f64(),
            Some(25.0)
        );
    }
}
