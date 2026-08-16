//! Route matching: method + path expression, optionally narrowed by the
//! request's query parameters (spec §5.2).
//!
//! Config routes are compiled once at startup into a [`RouteTable`] with their
//! upstream URLs parsed and their path expressions compiled. Matching is by HTTP
//! method membership, path, and (where a route declares them) query-parameter
//! presence conditions.
//!
//! Paths are matched in **two tiers**: every [`path_template`] route first, then
//! every [`path_prefix`] route. A template names one exact shape, a prefix names
//! a subtree, so the specific must be consulted before the general or a
//! catch-all would swallow the refinement written to escape it. Within the
//! template tier the fewest parameters win (the more literal template is the
//! narrower one); within the prefix tier the longest prefix wins. In both tiers
//! a query-conditioned route beats an unconditioned one at an equal key, and
//! config order is the final stable tiebreak. Every pair of routes that could
//! make this ordering arbitrary — two co-matchable templates where neither is
//! narrower, a template half-overlapping a prefix, two conditioned routes that a
//! single request could satisfy — is rejected at load time
//! ([`crate::config::validate`]), so this ordering always has exactly one
//! answer.
//!
//! [`path_template`]: crate::config::model::RouteMatch::path_template
//! [`path_prefix`]: crate::config::model::RouteMatch::path_prefix

use std::borrow::Cow;
use std::sync::Arc;

use thiserror::Error;
use url::Url;

use crate::config::model::{Config, RolloutConfig, RouteMatch, RouteMode, TimeoutsConfig};
use crate::contract::model::ComparisonRules;
use crate::resilience::CircuitBreaker;
use crate::routing::template::{CompiledTemplate, TemplateParseError};

/// Failure compiling a route into the route table.
#[derive(Debug, Error)]
pub enum RouteBuildError {
    /// An upstream URL did not parse (should not happen post-validation).
    #[error("route {id:?}: invalid upstream URL {url:?}: {source}")]
    BadUrl {
        /// The route id.
        id: String,
        /// The offending URL string.
        url: String,
        /// The parse error.
        source: url::ParseError,
    },
    /// A path template did not parse (should not happen post-validation).
    #[error("route {id:?}: invalid path template {template:?}: {source}")]
    BadTemplate {
        /// The route id.
        id: String,
        /// The offending template string.
        template: String,
        /// The parse error.
        source: TemplateParseError,
    },
    /// The match set both path fields or neither (should not happen
    /// post-validation).
    #[error("route {id:?}: match must set exactly one of path_prefix or path_template")]
    AmbiguousPathMatch {
        /// The route id.
        id: String,
    },
    /// The resolved-comparison list did not match the route count.
    #[error("internal error: {0} comparisons for {1} routes")]
    ComparisonCountMismatch(usize, usize),
}

/// A route's resolved comparison policy: the operational gate plus the merged
/// behavioral rules the comparison engine consumes (spec §4.4). Resolved once at
/// startup from the contract reference or inline rules.
#[derive(Debug, Clone)]
pub struct RouteComparison {
    /// Whether comparison is enabled for this route.
    pub enabled: bool,
    /// Fraction of eligible requests to buffer and compare.
    pub sample_rate: f64,
    /// Skip comparison above this body size.
    pub max_body_bytes: usize,
    /// Uppercased write methods this route opts into shadowing (spec §6.1).
    /// Empty for every route that has not opted in — reads stay the only
    /// shadow-eligible methods (safety invariant 3).
    pub shadow_methods: Vec<String>,
    /// Merged behavioral rules (what to compare and how).
    pub rules: ComparisonRules,
}

/// Marks a [`PathMatcher::basis`] string as naming a `path_prefix` route.
const PREFIX_BASIS: &str = "prefix:";
/// Marks a [`PathMatcher::basis`] string as naming a `path_template` route.
const TEMPLATE_BASIS: &str = "template:";

/// Whether a recorded [`PathMatcher::basis`] describes a matcher that folds
/// concrete paths — the read side of [`PathMatcher::observed_path`], for a
/// consumer holding the string a profile recorded rather than the matcher.
///
/// It lives here so the sigils stay private to the one module that writes them:
/// a reader that spelled `starts_with("template:")` itself would go on
/// compiling, and silently stop agreeing with `observed_path`, the first time
/// this enum grows a variant.
pub fn basis_normalizes_paths(basis: &str) -> bool {
    basis.starts_with(TEMPLATE_BASIS)
}

/// A route's compiled path expression. An enum rather than two optional fields
/// so "both" and "neither" are unrepresentable past `RouteTable::build` — the
/// matcher can then never face a route it has no way to test.
#[derive(Debug, Clone)]
pub enum PathMatcher {
    /// Everything under this prefix.
    Prefix(String),
    /// Exactly this shape.
    Template(CompiledTemplate),
}

impl PathMatcher {
    fn matches(&self, path: &str) -> bool {
        match self {
            Self::Prefix(prefix) => path.starts_with(prefix.as_str()),
            Self::Template(template) => template.matches_path(path),
        }
    }

    /// How this route names its paths, as one string: `prefix:/devices` or
    /// `template:/conversations/{id}`.
    ///
    /// The single definition of that wire form, which an observe profile
    /// records per route — see
    /// [`crate::observability::observe::RouteProfile::match_basis`] for why the
    /// matcher has to travel with the document it produced.
    pub fn basis(&self) -> String {
        match self {
            Self::Prefix(prefix) => format!("{PREFIX_BASIS}{prefix}"),
            Self::Template(template) => format!("{TEMPLATE_BASIS}{}", template.as_str()),
        }
    }

    /// The path a *distinct-path count* should be keyed on: the template's own
    /// text for a templated route, the raw path for a prefix route.
    ///
    /// Absorbing path cardinality is what a template is *for* — every
    /// `/conversations/<id>` is one operation, and counting the ids again
    /// after the operator has named the shape would report the one number the
    /// template was written to stop reporting. Prefix routes are untouched:
    /// under a prefix, path cardinality is still the only evidence available
    /// that the route is a subtree rather than an endpoint.
    pub fn observed_path<'a>(&'a self, path: &'a str) -> &'a str {
        match self {
            Self::Prefix(_) => path,
            Self::Template(template) => template.as_str(),
        }
    }

    /// Compile a config match's path expression. Also the config validator's
    /// view of a route's path (`crate::config::validate`), so the overlap pass
    /// and the matcher cannot disagree about what a match block names.
    pub(crate) fn compile(id: &str, m: &RouteMatch) -> Result<Self, RouteBuildError> {
        match (&m.path_prefix, &m.path_template) {
            (Some(prefix), None) => Ok(Self::Prefix(prefix.clone())),
            (None, Some(template)) => CompiledTemplate::parse(template)
                .map(Self::Template)
                .map_err(|source| RouteBuildError::BadTemplate {
                    id: id.to_string(),
                    template: template.clone(),
                    source,
                }),
            _ => Err(RouteBuildError::AmbiguousPathMatch { id: id.to_string() }),
        }
    }
}

/// A route compiled for matching and proxying.
#[derive(Debug, Clone)]
pub struct CompiledRoute {
    /// Route id (metric label, logs).
    pub id: String,
    /// Uppercased HTTP methods this route matches.
    pub methods: Vec<String>,
    /// The path expression this route matches.
    pub path: PathMatcher,
    /// Query parameter names that must all be present (empty = unconditioned).
    pub query_present: Vec<String>,
    /// Query parameter names of which none may be present (empty =
    /// unconditioned).
    pub query_absent: Vec<String>,
    /// Whether this route declares any query condition, derived at compile time
    /// from [`crate::config::model::RouteMatch::is_query_conditioned`] — the one
    /// definition, so the equal-prefix tiebreak below and the load-time
    /// disjointness check cannot disagree about which routes compete (spec §5.2).
    pub query_conditioned: bool,
    /// The route mode.
    pub mode: RouteMode,
    /// Parsed legacy upstream origin, if configured.
    pub legacy_upstream: Option<Url>,
    /// Parsed new upstream origin, if configured.
    pub new_upstream: Option<Url>,
    /// Per-route timeouts.
    pub timeouts: TimeoutsConfig,
    /// Resolved comparison policy.
    pub comparison: RouteComparison,
    /// Rollout settings (`percentage_split` only).
    pub rollout: Option<RolloutConfig>,
    /// Whether a failed in-flight request may be replayed against legacy
    /// (idempotent routes only; spec §6.5).
    pub failover_safe: bool,
    /// Per-route circuit breaker guarding the new upstream (`None` if disabled).
    pub breaker: Option<Arc<CircuitBreaker>>,
}

impl CompiledRoute {
    /// The rollout this route resolves a target percentage from, or `None` if it
    /// has no rollout to report.
    ///
    /// The one definition of "this route has a rollout target": the scrape
    /// handler that sets the gauge and the startup pass that pre-registers it
    /// ask the same question here, so they cannot come to different answers
    /// about which routes own a `limen_rollout_resolved_target_percentage`
    /// series.
    pub fn rollout_target(&self) -> Option<&RolloutConfig> {
        match self.mode {
            RouteMode::PercentageSplit => self.rollout.as_ref(),
            _ => None,
        }
    }

    /// Whether a request on this route can ever consult the circuit breaker —
    /// whether it both *has* one and runs in a mode that reaches `gate_new`
    /// (see [`crate::routing::decision::decide_primary`]).
    ///
    /// Config validation accepts a `circuit_breaker:` block on any mode, so
    /// "has a breaker" and "uses a breaker" are different questions, and only
    /// the second may pre-register transition series
    /// ([`crate::observability::prometheus::register_rollout_series`]). A route
    /// whose breaker is never asked would otherwise advertise four counters
    /// that cannot move — which reads on a dashboard exactly like a breaker
    /// that has never had to.
    ///
    /// Matched exhaustively, unlike [`rollout_target`](Self::rollout_target):
    /// there is no safe default for a new mode here, so one has to declare
    /// which side of `gate_new` it falls on.
    pub fn breaker_consulted(&self) -> bool {
        self.breaker.is_some()
            && match self.mode {
                RouteMode::PercentageSplit | RouteMode::FailoverToLegacy => true,
                RouteMode::LegacyOnly | RouteMode::NewOnly | RouteMode::ShadowLegacyPrimary => {
                    false
                }
            }
    }

    /// Whether this route matches the given (already-uppercased) method, path,
    /// and query-parameter names.
    fn matches(&self, method: &str, path: &str, query: &QueryNames<'_>) -> bool {
        self.methods.iter().any(|m| m == method)
            && self.path.matches(path)
            && self.query_conditions_hold(query)
    }

    /// Ordering key: template tier before prefix tier, then specificity within
    /// the tier, then a query-conditioned route before an unconditioned one.
    /// `sort_by_key` is stable, so config order remains the final tiebreak.
    fn sort_key(&self) -> (u8, usize, bool) {
        match &self.path {
            // Fewer parameters = more literal segments = narrower. Load-time
            // validation guarantees that of any two templates a single request
            // could match, one is strictly narrower than the other (or the two
            // are the same shape, which the query-condition key then orders) —
            // and the narrower one always has strictly fewer parameters. So this
            // is a total order over exactly the routes that compete.
            PathMatcher::Template(t) => (0, t.param_count(), !self.query_conditioned),
            // Longest prefix first, written as a descending length so one key
            // type covers both tiers.
            PathMatcher::Prefix(p) => (1, usize::MAX - p.len(), !self.query_conditioned),
        }
    }

    /// Presence only — `?prompt=` counts exactly like `?prompt=login`.
    fn query_conditions_hold(&self, query: &QueryNames<'_>) -> bool {
        self.query_present.iter().all(|n| query.contains(n))
            && !self.query_absent.iter().any(|n| query.contains(n))
    }
}

/// The parameter *names* in a request's query string, parsed once per request.
///
/// Decoding matches [`Url::query_pairs`] — same `form_urlencoded` parser,
/// exactly as the comparison engine reads query parameters. The decoding is
/// one-directional by design: a route naming `login_verifier` also matches a
/// request spelling it `login%5Fverifier`, but a *config* name is a literal and
/// is never decoded, so an encoded spelling there would match nothing. Config
/// names carrying `%`, `+`, or edge whitespace are therefore refused at load
/// time ([`crate::config::validate`]) rather than silently matching nothing.
/// A `Vec` rather than a set: conditions name one or two parameters and queries
/// carry a handful, so scanning is cheaper than hashing every name, and repeats
/// are irrelevant to a presence test.
struct QueryNames<'a>(Vec<Cow<'a, str>>);

impl<'a> QueryNames<'a> {
    /// `None` — including the "no route conditions on the query" case — yields
    /// the empty, non-allocating set without touching the query at all.
    fn parse(query: Option<&'a str>) -> Self {
        Self(
            query
                .into_iter()
                .flat_map(|q| url::form_urlencoded::parse(q.as_bytes()).map(|(name, _)| name))
                .collect(),
        )
    }

    fn contains(&self, name: &str) -> bool {
        self.0.iter().any(|n| n.as_ref() == name)
    }
}

/// The compiled routing table, ordered most-specific-first: the template tier
/// (fewest parameters first) ahead of the prefix tier (longest prefix first).
#[derive(Debug, Clone, Default)]
pub struct RouteTable {
    routes: Vec<CompiledRoute>,
    /// Whether any route declares a query condition; when false, matching skips
    /// parsing the query entirely and behaves exactly as it did before the
    /// fields existed.
    any_query_conditions: bool,
}

impl RouteTable {
    /// Compile the config's routes into a table: parse upstream URLs and pair
    /// each route with its already-resolved comparison policy (see
    /// [`super::resolve`]). Pure — no contract I/O happens here.
    pub fn build(
        config: &Config,
        comparisons: Vec<RouteComparison>,
    ) -> Result<Self, RouteBuildError> {
        if comparisons.len() != config.routes.len() {
            return Err(RouteBuildError::ComparisonCountMismatch(
                comparisons.len(),
                config.routes.len(),
            ));
        }
        let mut routes = Vec::with_capacity(config.routes.len());
        for (r, comparison) in config.routes.iter().zip(comparisons) {
            routes.push(CompiledRoute {
                id: r.id.clone(),
                methods: r
                    .r#match
                    .methods
                    .iter()
                    .map(|m| m.to_ascii_uppercase())
                    .collect(),
                path: PathMatcher::compile(&r.id, &r.r#match)?,
                query_present: r.r#match.query_present.clone(),
                query_absent: r.r#match.query_absent.clone(),
                query_conditioned: r.r#match.is_query_conditioned(),
                mode: r.mode,
                legacy_upstream: parse_opt(&r.id, r.legacy_upstream.as_deref())?,
                new_upstream: parse_opt(&r.id, r.new_upstream.as_deref())?,
                timeouts: r.timeouts.clone(),
                comparison,
                rollout: r.rollout.clone(),
                failover_safe: r.failover_safe,
                breaker: r
                    .circuit_breaker
                    .enabled
                    .then(|| Arc::new(CircuitBreaker::new(&r.id, &r.circuit_breaker))),
            });
        }
        let any_query_conditions = routes.iter().any(|r| r.query_conditioned);
        // Most specific first (see `sort_key`, and the tier rules in this
        // module's header). Two equal-length prefixes that both match a path are
        // necessarily the same prefix, so ordering by length groups exactly the
        // prefix routes that compete.
        routes.sort_by_key(CompiledRoute::sort_key);
        Ok(Self {
            routes,
            any_query_conditions,
        })
    }

    /// Find the best-matching route for a method + path + raw query string
    /// (the URI's query, still percent-encoded, as `Uri::query` returns it), or
    /// `None`.
    pub fn match_route(
        &self,
        method: &str,
        path: &str,
        query: Option<&str>,
    ) -> Option<&CompiledRoute> {
        // Parsed once per request rather than per candidate route — and not at
        // all unless some route actually conditions on the query.
        let names = QueryNames::parse(query.filter(|_| self.any_query_conditions));
        self.routes.iter().find(|r| r.matches(method, path, &names))
    }

    /// Iterate the compiled routes (e.g. to sample per-route breaker state).
    pub fn iter(&self) -> impl Iterator<Item = &CompiledRoute> {
        self.routes.iter()
    }

    /// Number of compiled routes.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether the table has no routes.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

fn parse_opt(id: &str, url: Option<&str>) -> Result<Option<Url>, RouteBuildError> {
    match url {
        Some(s) => Url::parse(s)
            .map(Some)
            .map_err(|source| RouteBuildError::BadUrl {
                id: id.to_string(),
                url: s.to_string(),
                source,
            }),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::resolve::resolve_comparisons;

    fn table(yaml: &str) -> RouteTable {
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let comparisons = resolve_comparisons(&config, std::path::Path::new(".")).unwrap();
        RouteTable::build(&config, comparisons).unwrap()
    }

    const ROUTES: &str = r#"
routes:
  - id: get-device
    match: { methods: ["GET"], path_prefix: "/devices/" }
    legacy_upstream: "https://legacy.internal"
    new_upstream: "https://new.internal"
    mode: shadow_legacy_primary
  - id: list-devices
    match: { methods: ["GET"], path_prefix: "/devices" }
    legacy_upstream: "https://legacy.internal"
    new_upstream: "https://new.internal"
    mode: legacy_only
  - id: create-device
    match: { methods: ["POST"], path_prefix: "/devices" }
    legacy_upstream: "https://legacy.internal"
    new_upstream: "https://new.internal"
    mode: new_only
"#;

    /// The field case (spec §5.2): `/oauth2/auth` split so the one-time-token
    /// hops relay uncompared while the plain authorize bounces stay compared.
    const OAUTH_ROUTES: &str = r#"
routes:
  - id: oauth-verifier
    match:
      methods: ["GET"]
      path_prefix: "/oauth2/auth"
      query_present: ["login_verifier"]
    legacy_upstream: "https://legacy.internal"
    mode: legacy_only
  - id: oauth-authorize
    match: { methods: ["GET"], path_prefix: "/oauth2/auth" }
    legacy_upstream: "https://legacy.internal"
    new_upstream: "https://new.internal"
    mode: shadow_legacy_primary
"#;

    #[test]
    fn longest_prefix_wins() {
        let t = table(ROUTES);
        assert_eq!(
            t.match_route("GET", "/devices/123", None).unwrap().id,
            "get-device"
        );
        // `/devices` (no trailing slash) only matches the shorter prefix.
        assert_eq!(
            t.match_route("GET", "/devices", None).unwrap().id,
            "list-devices"
        );
    }

    #[test]
    fn method_is_part_of_matching() {
        let t = table(ROUTES);
        // POST /devices/123 does not match the GET routes; it matches the POST
        // route on the `/devices` prefix.
        assert_eq!(
            t.match_route("POST", "/devices/123", None).unwrap().id,
            "create-device"
        );
        assert!(t.match_route("DELETE", "/devices/123", None).is_none());
    }

    #[test]
    fn no_match_returns_none() {
        let t = table(ROUTES);
        assert!(t.match_route("GET", "/widgets", None).is_none());
    }

    #[test]
    fn empty_table() {
        let t = table("routes: []");
        assert!(t.is_empty());
        assert!(t.match_route("GET", "/", None).is_none());
    }

    /// A table without query conditions must route identically whatever the
    /// request's query is — the fields are inert unless declared.
    #[test]
    fn unconditioned_routes_ignore_the_query() {
        let t = table(ROUTES);
        for query in [None, Some(""), Some("login_verifier=abc&x=1")] {
            assert_eq!(
                t.match_route("GET", "/devices/123", query).unwrap().id,
                "get-device"
            );
            assert!(t.match_route("GET", "/widgets", query).is_none());
        }
    }

    #[test]
    fn query_conditioned_route_beats_unconditioned_at_equal_prefix() {
        let t = table(OAUTH_ROUTES);
        assert_eq!(
            t.match_route("GET", "/oauth2/auth", Some("login_verifier=tok"))
                .unwrap()
                .id,
            "oauth-verifier"
        );
        // Without the parameter the conditioned route drops out and the plain
        // authorize route (which is the one being compared) matches.
        assert_eq!(
            t.match_route("GET", "/oauth2/auth", Some("client_id=app&scope=openid"))
                .unwrap()
                .id,
            "oauth-authorize"
        );
        assert_eq!(
            t.match_route("GET", "/oauth2/auth", None).unwrap().id,
            "oauth-authorize"
        );
    }

    /// Config order decides among conditioned routes only *after* prefix length:
    /// a longer unconditioned prefix still outranks a shorter conditioned one.
    #[test]
    fn longest_prefix_still_beats_a_shorter_conditioned_route() {
        let t = table(
            r#"
routes:
  - id: conditioned-short
    match:
      methods: ["GET"]
      path_prefix: "/oauth2"
      query_present: ["login_verifier"]
    legacy_upstream: "https://legacy.internal"
    mode: legacy_only
  - id: unconditioned-long
    match: { methods: ["GET"], path_prefix: "/oauth2/auth" }
    legacy_upstream: "https://legacy.internal"
    mode: legacy_only
"#,
        );
        assert_eq!(
            t.match_route("GET", "/oauth2/auth", Some("login_verifier=tok"))
                .unwrap()
                .id,
            "unconditioned-long"
        );
        // On the shorter path only the conditioned route is in scope.
        assert_eq!(
            t.match_route("GET", "/oauth2/token", Some("login_verifier=tok"))
                .unwrap()
                .id,
            "conditioned-short"
        );
    }

    #[test]
    fn query_present_requires_every_name() {
        let t = table(
            r#"
routes:
  - id: both
    match:
      methods: ["GET"]
      path_prefix: "/x"
      query_present: ["a", "b"]
    legacy_upstream: "https://legacy.internal"
    mode: legacy_only
"#,
        );
        assert_eq!(
            t.match_route("GET", "/x", Some("a=1&b=2")).unwrap().id,
            "both"
        );
        // AND semantics: one of the two is not enough.
        assert!(t.match_route("GET", "/x", Some("a=1")).is_none());
        assert!(t.match_route("GET", "/x", Some("b=2&c=3")).is_none());
        // Presence, not value: an empty (or bare) parameter still counts.
        assert_eq!(t.match_route("GET", "/x", Some("a=&b")).unwrap().id, "both");
        // Percent-encoded names decode before the containment check, matching
        // how the comparison engine reads query parameters.
        assert_eq!(
            t.match_route("GET", "/x", Some("%61=1&b=2")).unwrap().id,
            "both"
        );
    }

    /// The decoding asymmetry, stated once: the *request* side is decoded before
    /// the containment check, the *config* side never is. A literal config name
    /// therefore matches any encoded spelling of it, while an encoded config name
    /// would match nothing — which is why validation refuses to accept one.
    #[test]
    fn a_literal_config_name_matches_a_percent_encoded_request_name() {
        let t = table(
            r#"
routes:
  - id: literal
    match:
      methods: ["GET"]
      path_prefix: "/x"
      query_present: ["a"]
      query_absent: ["b"]
    legacy_upstream: "https://legacy.internal"
    mode: legacy_only
"#,
        );
        // `%61` is `a`, `%62` is `b`: both conditions see the decoded name.
        assert_eq!(
            t.match_route("GET", "/x", Some("%61=1")).unwrap().id,
            "literal"
        );
        assert!(t.match_route("GET", "/x", Some("a=1&%62=2")).is_none());
    }

    /// The reason templates exist: one path under a prefix behaves differently
    /// from its siblings, and no prefix can say so. The all-literal template is
    /// the narrower of the two (no parameters) and is consulted first.
    #[test]
    fn an_all_literal_template_beats_a_parameterized_one() {
        let yaml = r#"
routes:
  - id: by-id
    match: { methods: ["GET"], path_template: "/conversations/{id}" }
    legacy_upstream: "https://legacy.internal"
    mode: legacy_only
  - id: export
    match: { methods: ["GET"], path_template: "/conversations/export" }
    legacy_upstream: "https://legacy.internal"
    mode: legacy_only
"#;
        let t = table(yaml);
        assert_eq!(
            t.match_route("GET", "/conversations/export", None)
                .unwrap()
                .id,
            "export"
        );
        assert_eq!(
            t.match_route("GET", "/conversations/123", None).unwrap().id,
            "by-id"
        );
    }

    /// Config order is the last tiebreak, never the first: the specific route
    /// wins from either position in the file.
    #[test]
    fn swapping_config_order_does_not_change_the_winner() {
        let route = |id: &str, path: &str| {
            format!(
                "  - id: {id}\n    match: {{ methods: [\"GET\"], path_template: \"{path}\" }}\n    \
                 legacy_upstream: \"https://legacy.internal\"\n    mode: legacy_only\n"
            )
        };
        let export = route("export", "/conversations/export");
        let by_id = route("by-id", "/conversations/{id}");
        for yaml in [
            format!("routes:\n{export}{by_id}"),
            format!("routes:\n{by_id}{export}"),
        ] {
            let t = table(&yaml);
            assert_eq!(
                t.match_route("GET", "/conversations/export", None)
                    .unwrap()
                    .id,
                "export",
                "{yaml}"
            );
        }
    }

    const TEMPLATE_AND_CATCH_ALL: &str = r#"
routes:
  - id: conversations-all
    match: { methods: ["GET"], path_prefix: "/conversations/" }
    legacy_upstream: "https://legacy.internal"
    mode: legacy_only
  - id: conversation
    match: { methods: ["GET"], path_template: "/conversations/{id}" }
    legacy_upstream: "https://legacy.internal"
    mode: legacy_only
  - id: root
    match: { methods: ["GET"], path_prefix: "/" }
    legacy_upstream: "https://legacy.internal"
    mode: legacy_only
"#;

    /// The whole tier rule in one table: the template refines the prefix that
    /// contains it, and the prefix still catches everything the template's
    /// exact shape does not.
    #[test]
    fn a_template_beats_the_prefix_that_contains_it() {
        let t = table(TEMPLATE_AND_CATCH_ALL);
        assert_eq!(
            t.match_route("GET", "/conversations/123", None).unwrap().id,
            "conversation"
        );
        // Deeper than the template's shape: back to the prefix tier.
        assert_eq!(
            t.match_route("GET", "/conversations/1/2/3", None)
                .unwrap()
                .id,
            "conversations-all"
        );
        // Shallower, and outside both.
        assert_eq!(
            t.match_route("GET", "/conversations/", None).unwrap().id,
            "conversations-all"
        );
        assert_eq!(t.match_route("GET", "/voices/1", None).unwrap().id, "root");
    }

    /// A request whose path carries an empty segment or a trailing slash never
    /// matches a template — it falls to the prefix tier, which is where a path
    /// Limen cannot name belongs.
    #[test]
    fn a_path_a_template_cannot_name_falls_to_the_prefix_tier() {
        let t = table(TEMPLATE_AND_CATCH_ALL);
        assert_eq!(
            t.match_route("GET", "/conversations//preview", None)
                .unwrap()
                .id,
            "conversations-all"
        );
        assert_eq!(
            t.match_route("GET", "/conversations/123/", None)
                .unwrap()
                .id,
            "conversations-all"
        );
    }

    /// No percent-decoding on the template side: `%2F` is one character of one
    /// segment, so it cannot smuggle a request into a deeper shape.
    #[test]
    fn template_matching_does_not_percent_decode_the_path() {
        let t = table(TEMPLATE_AND_CATCH_ALL);
        assert_eq!(
            t.match_route("GET", "/conversations/1%2F2", None)
                .unwrap()
                .id,
            "conversation"
        );
        assert_eq!(
            t.match_route("GET", "/conversations/1%2F2/3", None)
                .unwrap()
                .id,
            "conversations-all"
        );
    }

    /// The field case again (spec §5.2), now with a template on one side: the
    /// conditioned prefix route keeps the verifier hops it was written to
    /// except, and the template takes everything else on its shape.
    #[test]
    fn a_conditioned_prefix_route_survives_a_complementary_template() {
        let t = table(
            r#"
routes:
  - id: oauth-verifier
    match:
      methods: ["GET"]
      path_prefix: "/oauth2/auth"
      query_present: ["login_verifier"]
    legacy_upstream: "https://legacy.internal"
    mode: legacy_only
  - id: oauth-action
    match:
      methods: ["GET"]
      path_template: "/oauth2/{action}"
      query_absent: ["login_verifier"]
    legacy_upstream: "https://legacy.internal"
    new_upstream: "https://new.internal"
    mode: shadow_legacy_primary
"#,
        );
        // The template is consulted first but its query condition fails, so the
        // request falls through to the conditioned prefix route.
        assert_eq!(
            t.match_route("GET", "/oauth2/auth", Some("login_verifier=tok"))
                .unwrap()
                .id,
            "oauth-verifier"
        );
        assert_eq!(
            t.match_route("GET", "/oauth2/auth", Some("client_id=app"))
                .unwrap()
                .id,
            "oauth-action"
        );
        assert_eq!(
            t.match_route("GET", "/oauth2/token", None).unwrap().id,
            "oauth-action"
        );
    }

    /// Two routes on the same shape: the conditioned one is consulted first,
    /// exactly as at an equal `path_prefix`.
    #[test]
    fn a_conditioned_template_beats_an_unconditioned_one_on_the_same_shape() {
        let t = table(
            r#"
routes:
  - id: plain
    match: { methods: ["GET"], path_template: "/oauth2/{action}" }
    legacy_upstream: "https://legacy.internal"
    mode: legacy_only
  - id: verifier
    match:
      methods: ["GET"]
      path_template: "/oauth2/{action}"
      query_present: ["login_verifier"]
    legacy_upstream: "https://legacy.internal"
    mode: legacy_only
"#,
        );
        assert_eq!(
            t.match_route("GET", "/oauth2/auth", Some("login_verifier=tok"))
                .unwrap()
                .id,
            "verifier"
        );
        assert_eq!(
            t.match_route("GET", "/oauth2/auth", None).unwrap().id,
            "plain"
        );
    }

    /// Templates of different segment counts are disjoint by construction, so
    /// both can sit in the table and each takes its own shape.
    #[test]
    fn templates_of_different_lengths_do_not_compete() {
        let t = table(
            r#"
routes:
  - id: one
    match: { methods: ["GET"], path_template: "/a/{x}" }
    legacy_upstream: "https://legacy.internal"
    mode: legacy_only
  - id: two
    match: { methods: ["GET"], path_template: "/a/{x}/{y}" }
    legacy_upstream: "https://legacy.internal"
    mode: legacy_only
"#,
        );
        assert_eq!(t.match_route("GET", "/a/1", None).unwrap().id, "one");
        assert_eq!(t.match_route("GET", "/a/1/2", None).unwrap().id, "two");
        assert!(t.match_route("GET", "/a", None).is_none());
    }

    /// The two strings an observe profile is built from: the basis it records
    /// per route, and the key its distinct-path count is taken over.
    #[test]
    fn a_matcher_names_its_basis_and_normalizes_only_a_template() {
        let t = table(TEMPLATE_AND_CATCH_ALL);
        let route = |id: &str| t.iter().find(|r| r.id == id).expect("route");

        let templated = &route("conversation").path;
        assert_eq!(templated.basis(), "template:/conversations/{id}");
        // Every id keys to the shape, which is what makes the count "how many
        // operations" rather than "how many conversations".
        assert_eq!(
            templated.observed_path("/conversations/123"),
            "/conversations/{id}"
        );
        assert_eq!(
            templated.observed_path("/conversations/456"),
            templated.observed_path("/conversations/123")
        );

        let prefixed = &route("conversations-all").path;
        assert_eq!(prefixed.basis(), "prefix:/conversations/");
        // Untouched: under a prefix the concrete path is the only evidence
        // available that the route is a subtree rather than an endpoint.
        assert_eq!(
            prefixed.observed_path("/conversations/123"),
            "/conversations/123"
        );
    }

    #[test]
    fn a_template_route_still_matches_on_method() {
        let t = table(TEMPLATE_AND_CATCH_ALL);
        assert!(t.match_route("POST", "/conversations/1", None).is_none());
    }

    #[test]
    fn query_absent_rejects_any_named_parameter() {
        let t = table(
            r#"
routes:
  - id: plain
    match:
      methods: ["GET"]
      path_prefix: "/x"
      query_absent: ["login_verifier", "consent_verifier"]
    legacy_upstream: "https://legacy.internal"
    mode: legacy_only
"#,
        );
        assert_eq!(t.match_route("GET", "/x", None).unwrap().id, "plain");
        assert_eq!(
            t.match_route("GET", "/x", Some("client_id=a")).unwrap().id,
            "plain"
        );
        assert!(t
            .match_route("GET", "/x", Some("login_verifier=tok"))
            .is_none());
        assert!(t
            .match_route("GET", "/x", Some("a=1&consent_verifier="))
            .is_none());
    }
}
