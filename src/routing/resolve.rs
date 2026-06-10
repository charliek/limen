//! Resolving each route's comparison policy at startup (spec §4.4).
//!
//! This is the single place that loads contracts and merges behavioral rules
//! into the resolved [`RouteComparison`] the proxy consumes. Keeping it separate
//! from [`super::matcher`] leaves route *matching* a pure, I/O-free concern and
//! gives later phases one home for per-route policy resolution.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::model::{Config, RouteConfig};
use crate::contract::load as contract_load;
use crate::contract::merge;
use crate::contract::model::{BehavioralRules, Contract};
use crate::routing::matcher::RouteComparison;

/// Failure resolving a route's comparison policy (should not happen
/// post-validation, which loads and checks every contract).
#[derive(Debug, Error)]
#[error("route {id:?}: {message}")]
pub struct ResolveError {
    /// The route id.
    pub id: String,
    /// The resolution error.
    pub message: String,
}

/// Resolve every route's comparison policy, in `config.routes` order. Contracts
/// are loaded once per distinct file. `base_dir` resolves relative references.
pub fn resolve_comparisons(
    config: &Config,
    base_dir: &Path,
) -> Result<Vec<RouteComparison>, ResolveError> {
    let mut contracts: HashMap<PathBuf, Contract> = HashMap::new();
    config
        .routes
        .iter()
        .map(|route| resolve_one(route, base_dir, &mut contracts))
        .collect()
}

fn resolve_one(
    route: &RouteConfig,
    base_dir: &Path,
    contracts: &mut HashMap<PathBuf, Contract>,
) -> Result<RouteComparison, ResolveError> {
    let err = |message: String| ResolveError {
        id: route.id.clone(),
        message,
    };

    let rules = if let Some(reference) = &route.contract {
        let parsed = contract_load::parse_ref(reference).map_err(|e| err(e.to_string()))?;
        let path = contract_load::resolve_path(base_dir, &parsed.file);
        if !contracts.contains_key(&path) {
            let loaded = contract_load::load_file(&path).map_err(|e| err(e.to_string()))?;
            contracts.insert(path.clone(), loaded);
        }
        let contract = &contracts[&path];
        let entry = contract.route(&parsed.route_id).ok_or_else(|| {
            err(format!(
                "contract {} has no route {:?}",
                path.display(),
                parsed.route_id
            ))
        })?;
        let empty = BehavioralRules::default();
        merge::resolve(
            &contract.defaults,
            entry.comparison.as_ref().unwrap_or(&empty),
        )
    } else {
        let inline = route.comparison.inline_behavioral();
        merge::resolve(&BehavioralRules::default(), &inline)
    };

    Ok(RouteComparison {
        enabled: route.comparison.enabled,
        sample_rate: route.comparison.sample_rate,
        max_body_bytes: route.comparison.max_body_bytes as usize,
        rules,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_inline_and_default_rules() {
        let config: Config = serde_yaml::from_str(
            r#"
routes:
  - id: a
    match: { methods: ["GET"], path_prefix: "/a" }
    legacy_upstream: "https://l"
    mode: legacy_only
  - id: b
    match: { methods: ["GET"], path_prefix: "/b" }
    legacy_upstream: "https://l"
    new_upstream: "https://n"
    mode: shadow_legacy_primary
    comparison:
      enabled: true
      sample_rate: 0.5
      max_body_bytes: 1024
      json: { ignore_paths: ["$.ts"] }
"#,
        )
        .unwrap();
        let resolved = resolve_comparisons(&config, Path::new(".")).unwrap();
        assert_eq!(resolved.len(), 2);
        assert!(!resolved[0].enabled);
        assert!(resolved[1].enabled);
        assert_eq!(resolved[1].sample_rate, 0.5);
        assert_eq!(resolved[1].rules.json.ignore_paths, vec!["$.ts"]);
    }
}
