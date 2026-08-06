//! Route matching: method + longest-path-prefix, optionally narrowed by the
//! request's query parameters (spec §5.2).
//!
//! Config routes are compiled once at startup into a [`RouteTable`] with their
//! upstream URLs parsed. Matching is by HTTP method membership, path prefix, and
//! (where a route declares them) query-parameter presence conditions; among
//! matching routes the **longest prefix wins**, a query-conditioned route beats
//! an unconditioned one at an equal prefix, and config order is the final stable
//! tiebreak. Two conditioned routes that could both match one request are
//! rejected at load time ([`crate::config::validate`]), so this ordering always
//! has exactly one answer.

use std::borrow::Cow;
use std::sync::Arc;

use thiserror::Error;
use url::Url;

use crate::config::model::{Config, RolloutConfig, RouteMode, TimeoutsConfig};
use crate::contract::model::ComparisonRules;
use crate::resilience::CircuitBreaker;

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

/// A route compiled for matching and proxying.
#[derive(Debug, Clone)]
pub struct CompiledRoute {
    /// Route id (metric label, logs).
    pub id: String,
    /// Uppercased HTTP methods this route matches.
    pub methods: Vec<String>,
    /// Path prefix this route matches.
    pub path_prefix: String,
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
    /// Whether this route matches the given (already-uppercased) method, path,
    /// and query-parameter names.
    fn matches(&self, method: &str, path: &str, query: &QueryNames<'_>) -> bool {
        self.methods.iter().any(|m| m == method)
            && path.starts_with(&self.path_prefix)
            && self.query_conditions_hold(query)
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

/// The compiled routing table, ordered longest-prefix-first.
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
                path_prefix: r.r#match.path_prefix.clone(),
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
                    .then(|| Arc::new(CircuitBreaker::new(&r.circuit_breaker))),
            });
        }
        let any_query_conditions = routes.iter().any(|r| r.query_conditioned);
        // Longest prefix first, then query-conditioned before unconditioned so a
        // narrower route on the same prefix is consulted first (spec §5.2);
        // `sort_by_key` is stable, so routes with equal keys keep their config
        // order as a deterministic final tiebreak. Two equal-length prefixes that
        // both match a path are necessarily the same prefix, so ordering by
        // length groups exactly the routes that compete.
        routes.sort_by_key(|r| (std::cmp::Reverse(r.path_prefix.len()), !r.query_conditioned));
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
