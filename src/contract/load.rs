//! Loading contract files and resolving `path#routeId` references.
//!
//! A contract file is YAML or JSON, detected by extension (spec §4.2). A Limen
//! route references one route within it as `./contracts/svc.contract.yaml#get`.
//! Contracts are loaded once at startup and never hot-reloaded.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::compare::jsonpath;
use crate::contract::model::{BehavioralRules, Contract};

/// The only contract schema version the MVP supports.
pub const SUPPORTED_VERSION: u32 = 1;

/// Errors from loading or resolving a contract.
#[derive(Debug, Error)]
pub enum ContractError {
    /// The file could not be read.
    #[error("cannot read contract file {path}: {source}")]
    Io {
        /// The path that failed to read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The file extension is not one Limen recognizes.
    #[error("unrecognized contract extension for {path} (expected .yaml, .yml, or .json)")]
    UnknownExtension {
        /// The offending path.
        path: PathBuf,
    },
    /// The file did not parse against the contract schema.
    #[error("invalid contract {path}: {message}")]
    Parse {
        /// The path that failed to parse.
        path: PathBuf,
        /// A field-pathed parse error message.
        message: String,
    },
    /// A reference string was not of the form `path#routeId`.
    #[error("invalid contract reference {reference:?}: expected `path#routeId`")]
    BadReference {
        /// The reference string as written.
        reference: String,
    },
    /// The referenced route id is absent from the contract.
    #[error("contract {path} has no route {route_id:?}")]
    RouteNotFound {
        /// The contract file.
        path: PathBuf,
        /// The route id that was not found.
        route_id: String,
    },
}

/// A parsed `path#routeId` contract reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractRef {
    /// The contract file path, as written in the reference (may be relative).
    pub file: PathBuf,
    /// The route id (the fragment after `#`).
    pub route_id: String,
}

/// Parse a `path#routeId` reference. The split is on the last `#` so a path
/// may itself (unusually) contain one.
pub fn parse_ref(reference: &str) -> Result<ContractRef, ContractError> {
    match reference.rsplit_once('#') {
        Some((file, route_id)) if !file.is_empty() && !route_id.is_empty() => Ok(ContractRef {
            file: PathBuf::from(file),
            route_id: route_id.to_string(),
        }),
        _ => Err(ContractError::BadReference {
            reference: reference.to_string(),
        }),
    }
}

/// Resolve a reference's file path relative to `base_dir` (the config file's
/// directory). Absolute paths are returned unchanged.
pub fn resolve_path(base_dir: &Path, file: &Path) -> PathBuf {
    if file.is_absolute() {
        file.to_path_buf()
    } else {
        base_dir.join(file)
    }
}

/// Load and parse a contract file, choosing the parser by extension.
pub fn load_file(path: &Path) -> Result<Contract, ContractError> {
    let text = std::fs::read_to_string(path).map_err(|source| ContractError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    match ext.as_deref() {
        Some("yaml") | Some("yml") => {
            let de = serde_yaml::Deserializer::from_str(&text);
            serde_path_to_error::deserialize(de).map_err(|e| ContractError::Parse {
                path: path.to_path_buf(),
                message: e.to_string(),
            })
        }
        Some("json") => {
            let de = &mut serde_json::Deserializer::from_str(&text);
            serde_path_to_error::deserialize(de).map_err(|e| ContractError::Parse {
                path: path.to_path_buf(),
                message: e.to_string(),
            })
        }
        _ => Err(ContractError::UnknownExtension {
            path: path.to_path_buf(),
        }),
    }
}

/// A single JSONPath-subset violation found in a contract.
#[derive(Debug, Clone, PartialEq)]
pub struct PathIssue {
    /// The route id the path belongs to, or `None` for service defaults.
    pub route_id: Option<String>,
    /// The behavioral field that carried the path (`ignore_paths`, …).
    pub field: &'static str,
    /// The offending path string.
    pub path: String,
    /// Why it was rejected.
    pub error: jsonpath::JsonPathError,
}

/// Whether `layer` lists `header` in `compare_headers` (case-insensitively —
/// header names are case-insensitive on the wire).
fn lists_header(layer: &BehavioralRules, header: &str) -> bool {
    layer
        .compare_headers
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .any(|h| h.trim().eq_ignore_ascii_case(header))
}

/// Every misuse of `compare_headers` as a stand-in for an optional comparison
/// dimension that `layers`, taken together, commit, as `(header, block)` pairs.
/// `set_cookie`/`location` are separate comparison dimensions rather than
/// `compare_headers` entries — hence an error, not a warning (spec §4.2).
///
/// The two dimensions are deliberately **asymmetric**:
///
/// - `set-cookie` is an error *unconditionally*, block or no block. The generic
///   header path compares one `HeaderMap::get` value per name (see
///   `compare::compare_headers`), so a response carrying several `Set-Cookie`
///   headers silently loses all but the first — comparing cookies through it is
///   always a config bug. The dedicated `set_cookie` block is the only correct
///   tool, so listing the header never has a legitimate reading.
/// - `location` is a genuinely single-value header, so the generic path
///   compares it faithfully; only declaring it *alongside* a `location` block is
///   ambiguous intent. Listing it on its own stays legal.
///
/// Layers are considered together because they resolve together: a
/// `compare_headers` entry in service defaults conflicts with a block declared
/// per-route, and vice versa. Callers pass a single layer for inline rules
/// (spec §4.4) and `[defaults, route]` for a contract route.
pub fn header_dimension_conflicts(
    layers: &[&BehavioralRules],
) -> Vec<(&'static str, &'static str)> {
    let mut out = Vec::new();
    if layers.iter().any(|l| lists_header(l, "set-cookie")) {
        out.push(("set-cookie", "set_cookie"));
    }
    if layers.iter().any(|l| lists_header(l, "location"))
        && layers.iter().any(|l| l.location.is_some())
    {
        out.push(("location", "location"));
    }
    out
}

/// The canonical wording for a [`header_dimension_conflicts`] finding, so every
/// surface (contract check, inline route rules) reports it identically. Callers
/// prefix the subject (`route "x" …`, `` `defaults` … ``, or a config location).
pub fn header_dimension_conflict_message(header: &str, block: &str) -> String {
    if header.eq_ignore_ascii_case("set-cookie") {
        // Unconditional: there may be no block at all to point at, and the
        // generic path is lossy either way.
        return format!(
            "lists {header:?} in `compare_headers` — `{block}` is a separate comparison \
             dimension, and the generic header path compares a single value, silently \
             dropping the rest of a multi-cookie response; drop the `compare_headers` \
             entry and use a `{block}` block instead"
        );
    }
    format!(
        "lists {header:?} in `compare_headers` and also declares a `{block}` block — \
         `{block}` is a separate comparison dimension; drop the `compare_headers` entry"
    )
}

/// Report `compare_headers` dimension misuse across a contract: once at
/// `defaults` when it lives entirely there (it would otherwise repeat on every
/// route), else per affected route.
fn validate_header_dimension_conflicts(contract: &Contract, issues: &mut Vec<String>) {
    let empty = BehavioralRules::default();
    let in_defaults = header_dimension_conflicts(&[&contract.defaults]);
    for (header, block) in &in_defaults {
        issues.push(format!(
            "`defaults` {}",
            header_dimension_conflict_message(header, block)
        ));
    }
    for route in &contract.routes {
        let over = route.comparison.as_ref().unwrap_or(&empty);
        for (header, block) in header_dimension_conflicts(&[&contract.defaults, over]) {
            if in_defaults.contains(&(header, block)) {
                continue; // Already reported at its source.
            }
            issues.push(format!(
                "route {:?} {}",
                route.id,
                header_dimension_conflict_message(header, block)
            ));
        }
    }
}

/// Validate contract *semantics* beyond serde shape: the schema version is
/// supported, `service` is non-empty, route ids are non-empty and unique, and
/// no `compare_headers` entry usurps the `set_cookie`/`location` dimensions.
/// Returns human-readable messages (empty = valid). JSONPath-subset compliance
/// is reported separately by [`validate_paths`].
pub fn validate_semantics(contract: &Contract) -> Vec<String> {
    let mut issues = Vec::new();
    if contract.version != SUPPORTED_VERSION {
        issues.push(format!(
            "unsupported contract version {} (the MVP supports version {SUPPORTED_VERSION})",
            contract.version
        ));
    }
    if contract.service.trim().is_empty() {
        issues.push("`service` must not be empty".to_string());
    }
    let mut seen = HashSet::new();
    for route in &contract.routes {
        if route.id.trim().is_empty() {
            issues.push("a route has an empty `id`".to_string());
        } else if !seen.insert(route.id.as_str()) {
            issues.push(format!("duplicate route id {:?}", route.id));
        }
        if route.match_.path_template.trim().is_empty() {
            issues.push(format!(
                "route {:?} has an empty `match.path_template`",
                route.id
            ));
        }
    }
    validate_header_dimension_conflicts(contract, &mut issues);
    issues
}

/// Validate every JSONPath in a contract against the supported subset. An empty
/// result means the contract is fully compliant.
pub fn validate_paths(contract: &Contract) -> Vec<PathIssue> {
    let mut issues = Vec::new();

    let mut check = |route_id: Option<&str>, field: &'static str, path: &str| {
        if let Err(error) = jsonpath::parse(path) {
            issues.push(PathIssue {
                route_id: route_id.map(str::to_string),
                field,
                path: path.to_string(),
                error,
            });
        }
    };

    for (field, path) in contract.defaults.json_paths() {
        check(None, field, path);
    }
    for route in &contract.routes {
        if let Some(comparison) = &route.comparison {
            for (field, path) in comparison.json_paths() {
                check(Some(&route.id), field, path);
            }
        }
    }

    issues
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reference() {
        let r = parse_ref("./contracts/device.contract.yaml#get-device").unwrap();
        assert_eq!(r.file, PathBuf::from("./contracts/device.contract.yaml"));
        assert_eq!(r.route_id, "get-device");
    }

    #[test]
    fn rejects_reference_without_fragment() {
        assert!(parse_ref("./contracts/device.contract.yaml").is_err());
        assert!(parse_ref("#get-device").is_err());
        assert!(parse_ref("file#").is_err());
    }

    #[test]
    fn resolve_path_handles_relative_and_absolute() {
        let base = Path::new("/etc/limen");
        assert_eq!(
            resolve_path(base, Path::new("./c/x.yaml")),
            PathBuf::from("/etc/limen/./c/x.yaml")
        );
        assert_eq!(
            resolve_path(base, Path::new("/abs/x.yaml")),
            PathBuf::from("/abs/x.yaml")
        );
    }

    #[test]
    fn loads_yaml_and_json_equivalently() {
        let dir = tempfile::tempdir().unwrap();
        let yaml_path = dir.path().join("svc.contract.yaml");
        let json_path = dir.path().join("svc.contract.json");
        std::fs::write(
            &yaml_path,
            "version: 1\nservice: s\nroutes:\n  - id: r\n    match: { methods: [GET], path_template: \"/x\" }\n",
        )
        .unwrap();
        std::fs::write(
            &json_path,
            r#"{"version":1,"service":"s","routes":[{"id":"r","match":{"methods":["GET"],"path_template":"/x"}}]}"#,
        )
        .unwrap();
        let from_yaml = load_file(&yaml_path).unwrap();
        let from_json = load_file(&json_path).unwrap();
        assert_eq!(from_yaml, from_json);
    }

    #[test]
    fn unknown_extension_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svc.contract.txt");
        std::fs::write(&path, "version: 1\nservice: s\n").unwrap();
        assert!(matches!(
            load_file(&path),
            Err(ContractError::UnknownExtension { .. })
        ));
    }

    #[test]
    fn validate_semantics_flags_version_service_and_dup_ids() {
        let yaml = r#"
version: 2
service: ""
routes:
  - id: dup
    match: { methods: [GET], path_template: "/x" }
  - id: dup
    match: { methods: [GET], path_template: "/x" }
  - id: ""
    match: { methods: [GET], path_template: "/x" }
"#;
        let contract: Contract = serde_yaml::from_str(yaml).unwrap();
        let issues = validate_semantics(&contract);
        assert!(issues
            .iter()
            .any(|i| i.contains("unsupported contract version 2")));
        assert!(issues.iter().any(|i| i.contains("service")));
        assert!(issues.iter().any(|i| i.contains("duplicate route id")));
        assert!(issues.iter().any(|i| i.contains("empty `id`")));
    }

    #[test]
    fn validate_semantics_accepts_a_good_contract() {
        let yaml = "version: 1\nservice: s\nroutes:\n  - id: a\n    match: { methods: [GET], path_template: \"/a\" }\n  - id: b\n    match: { methods: [GET], path_template: \"/b\" }\n";
        let contract: Contract = serde_yaml::from_str(yaml).unwrap();
        assert!(validate_semantics(&contract).is_empty());
    }

    #[test]
    fn compare_headers_conflicting_with_a_route_block_is_an_error() {
        // `location` listed in defaults, block declared on the route: the
        // conflict only exists in the resolved route, so the route is named.
        let yaml = r#"
version: 1
service: s
defaults:
  compare_headers: ["Content-Type", "Location"]
routes:
  - id: r
    match: { methods: [POST], path_template: "/sessions" }
    comparison:
      location:
        origin: ignore
  - id: clean
    match: { methods: [GET], path_template: "/x" }
"#;
        let contract: Contract = serde_yaml::from_str(yaml).unwrap();
        let issues = validate_semantics(&contract);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("route \"r\""));
        assert!(issues[0].contains("location"));
    }

    #[test]
    fn compare_headers_set_cookie_alongside_a_block_is_an_error() {
        // Listing the header is rejected at its source — `defaults` here — and
        // a `set_cookie` block elsewhere changes nothing.
        let yaml = r#"
version: 1
service: s
defaults:
  compare_headers: ["Content-Type", "Set-Cookie"]
routes:
  - id: r
    match: { methods: [POST], path_template: "/sessions" }
    comparison:
      set_cookie:
        compare_values: presence
  - id: clean
    match: { methods: [GET], path_template: "/x" }
"#;
        let contract: Contract = serde_yaml::from_str(yaml).unwrap();
        let issues = validate_semantics(&contract);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("`defaults`"));
        assert!(issues[0].contains("set-cookie"));
        assert!(issues[0].contains("set_cookie"));
    }

    #[test]
    fn compare_headers_conflict_inside_defaults_is_reported_once() {
        let yaml = r#"
version: 1
service: s
defaults:
  compare_headers: ["location"]
  location:
    origin: ignore
routes:
  - id: a
    match: { methods: [GET], path_template: "/a" }
  - id: b
    match: { methods: [GET], path_template: "/b" }
"#;
        let contract: Contract = serde_yaml::from_str(yaml).unwrap();
        let issues = validate_semantics(&contract);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("`defaults`"));
        assert!(issues[0].contains("location"));
    }

    #[test]
    fn compare_headers_set_cookie_without_any_block_is_still_an_error() {
        // No `set_cookie` block anywhere: the generic header path would compare
        // a single value and drop the rest, so the entry is rejected on its own.
        let yaml = r#"
version: 1
service: s
defaults:
  compare_headers: ["content-type"]
routes:
  - id: r
    match: { methods: [POST], path_template: "/sessions" }
    comparison:
      compare_headers: ["Set-Cookie"]
  - id: clean
    match: { methods: [GET], path_template: "/x" }
"#;
        let contract: Contract = serde_yaml::from_str(yaml).unwrap();
        let issues = validate_semantics(&contract);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("route \"r\""));
        assert!(issues[0].contains("set-cookie"));
        assert!(issues[0].contains("`set_cookie` block"));
    }

    #[test]
    fn compare_headers_location_without_a_block_is_allowed() {
        // `location` is a genuine single-value header: listing it is only an
        // error when a `location` block is present too.
        let yaml = r#"
version: 1
service: s
defaults:
  compare_headers: ["Location"]
routes:
  - id: r
    match: { methods: [GET], path_template: "/x" }
"#;
        let contract: Contract = serde_yaml::from_str(yaml).unwrap();
        assert!(validate_semantics(&contract).is_empty());
    }

    #[test]
    fn a_block_without_the_header_entry_is_fine() {
        let yaml = r#"
version: 1
service: s
defaults:
  compare_headers: ["content-type"]
  set_cookie: {}
routes:
  - id: r
    match: { methods: [GET], path_template: "/x" }
    comparison:
      location:
        ignore_query_params: [state]
"#;
        let contract: Contract = serde_yaml::from_str(yaml).unwrap();
        assert!(validate_semantics(&contract).is_empty());
    }

    #[test]
    fn validate_paths_flags_out_of_subset() {
        let yaml = r#"
version: 1
service: s
defaults:
  json:
    ignore_paths: ["$.ok", "$.bad[0]"]
routes:
  - id: r
    match: { methods: [GET], path_template: "/x" }
    comparison:
      json:
        ignore_paths: ["$..deep"]
"#;
        let contract: Contract = serde_yaml::from_str(yaml).unwrap();
        let issues = validate_paths(&contract);
        assert_eq!(issues.len(), 2);
        assert!(issues
            .iter()
            .any(|i| i.path == "$.bad[0]" && i.route_id.is_none()));
        assert!(issues
            .iter()
            .any(|i| i.path == "$..deep" && i.route_id.as_deref() == Some("r")));
    }
}
