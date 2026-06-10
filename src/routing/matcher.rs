//! Route matching: method + longest-path-prefix (spec §5.2).
//!
//! Config routes are compiled once at startup into a [`RouteTable`] with their
//! upstream URLs parsed. Matching is by HTTP method membership and path prefix;
//! among matching routes the **longest prefix wins**, with config order as a
//! stable tiebreak.

use thiserror::Error;
use url::Url;

use crate::config::model::{Config, RouteMode, TimeoutsConfig};

/// Failure compiling a route's upstream URL into the route table.
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
    /// The route mode.
    pub mode: RouteMode,
    /// Parsed legacy upstream origin, if configured.
    pub legacy_upstream: Option<Url>,
    /// Parsed new upstream origin, if configured.
    pub new_upstream: Option<Url>,
    /// Per-route timeouts.
    pub timeouts: TimeoutsConfig,
}

impl CompiledRoute {
    /// Whether this route matches the given (already-uppercased) method and path.
    fn matches(&self, method: &str, path: &str) -> bool {
        self.methods.iter().any(|m| m == method) && path.starts_with(&self.path_prefix)
    }
}

/// The compiled routing table, ordered longest-prefix-first.
#[derive(Debug, Clone, Default)]
pub struct RouteTable {
    routes: Vec<CompiledRoute>,
}

impl RouteTable {
    /// Compile the config's routes into a table. URLs are parsed here.
    pub fn build(config: &Config) -> Result<Self, RouteBuildError> {
        let mut routes = Vec::with_capacity(config.routes.len());
        for r in &config.routes {
            routes.push(CompiledRoute {
                id: r.id.clone(),
                methods: r
                    .r#match
                    .methods
                    .iter()
                    .map(|m| m.to_ascii_uppercase())
                    .collect(),
                path_prefix: r.r#match.path_prefix.clone(),
                mode: r.mode,
                legacy_upstream: parse_opt(&r.id, r.legacy_upstream.as_deref())?,
                new_upstream: parse_opt(&r.id, r.new_upstream.as_deref())?,
                timeouts: r.timeouts.clone(),
            });
        }
        // Longest prefix first; `sort_by_key` is stable, so equal-length
        // prefixes keep their config order as a deterministic tiebreak.
        routes.sort_by_key(|r| std::cmp::Reverse(r.path_prefix.len()));
        Ok(Self { routes })
    }

    /// Find the best-matching route for a method + path, or `None`.
    pub fn match_route(&self, method: &str, path: &str) -> Option<&CompiledRoute> {
        self.routes.iter().find(|r| r.matches(method, path))
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

    fn table(yaml: &str) -> RouteTable {
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        RouteTable::build(&config).unwrap()
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

    #[test]
    fn longest_prefix_wins() {
        let t = table(ROUTES);
        assert_eq!(
            t.match_route("GET", "/devices/123").unwrap().id,
            "get-device"
        );
        // `/devices` (no trailing slash) only matches the shorter prefix.
        assert_eq!(t.match_route("GET", "/devices").unwrap().id, "list-devices");
    }

    #[test]
    fn method_is_part_of_matching() {
        let t = table(ROUTES);
        // POST /devices/123 does not match the GET routes; it matches the POST
        // route on the `/devices` prefix.
        assert_eq!(
            t.match_route("POST", "/devices/123").unwrap().id,
            "create-device"
        );
        assert!(t.match_route("DELETE", "/devices/123").is_none());
    }

    #[test]
    fn no_match_returns_none() {
        let t = table(ROUTES);
        assert!(t.match_route("GET", "/widgets").is_none());
    }

    #[test]
    fn empty_table() {
        let t = table("routes: []");
        assert!(t.is_empty());
        assert!(t.match_route("GET", "/").is_none());
    }
}
