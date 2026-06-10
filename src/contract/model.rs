//! Serde model for the shared behavioral contract (spec §4.2).
//!
//! The contract is the single source of truth for *comparison semantics* —
//! what to compare and how to normalize it — and is byte-for-byte portable
//! between Limen and Pharos. The behavioral vocabulary defined here
//! ([`BehavioralRules`], [`JsonRules`], …) is reused by the inline-rules
//! fallback in [`crate::config`], so a route's comparison config speaks exactly
//! the same language whether it references a contract or inlines its rules.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A complete contract file: service-wide `defaults` plus per-route overrides.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    /// Schema version; `1` for the MVP.
    pub version: u32,
    /// The service this contract describes (matches a route's expectations).
    pub service: String,
    /// Optional human description.
    #[serde(default)]
    pub description: Option<String>,
    /// Service-wide behavioral defaults; per-route `comparison` merges on top.
    #[serde(default)]
    pub defaults: BehavioralRules,
    /// The per-route entries a Limen/Pharos reference resolves against.
    #[serde(default)]
    pub routes: Vec<ContractRoute>,
}

impl Contract {
    /// Find a route entry by its `id` (the fragment of a `path#routeId` ref).
    pub fn route(&self, id: &str) -> Option<&ContractRoute> {
        self.routes.iter().find(|r| r.id == id)
    }
}

/// A single route entry within a contract.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContractRoute {
    /// Stable identifier referenced as `…contract.yaml#<id>`.
    pub id: String,
    /// Informational match metadata (methods + path template).
    #[serde(default, rename = "match")]
    pub match_: Option<ContractMatch>,
    /// Per-route behavioral overrides, merged onto the service defaults.
    #[serde(default)]
    pub comparison: Option<BehavioralRules>,
    /// Notes about typical status and intentional changes.
    #[serde(default)]
    pub expectations: Option<Expectations>,
    /// Free-form tags (`read`, `migration-ready`, …).
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

/// Informational route match metadata (not used for Limen's own routing).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContractMatch {
    /// HTTP methods this contract route describes.
    #[serde(default)]
    pub methods: Vec<String>,
    /// A path template such as `/devices/{id}`.
    #[serde(default)]
    pub path_template: Option<String>,
}

/// Notes a contract author records about a route.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Expectations {
    /// The status code the route typically returns.
    #[serde(default)]
    pub typical_status: Option<u16>,
    /// Free-form notes, e.g. documenting an intentional change.
    #[serde(default)]
    pub notes: Option<String>,
}

/// A mergeable *layer* of behavioral rules: every field is optional so service
/// defaults and per-route (or inline) overrides can be combined into a resolved
/// [`ComparisonRules`].
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BehavioralRules {
    /// Compare the HTTP status code (default `true`).
    #[serde(default)]
    pub compare_status: Option<bool>,
    /// Compare the normalized body (default `true`).
    #[serde(default)]
    pub compare_body: Option<bool>,
    /// Header names to compare; headers are compared only if listed.
    #[serde(default)]
    pub compare_headers: Option<Vec<String>>,
    /// JSON normalization rules.
    #[serde(default)]
    pub json: Option<JsonRules>,
}

impl BehavioralRules {
    /// Whether this layer declares *any* behavioral rule. Used to detect an
    /// inline behavioral block on a Limen route (which conflicts with a
    /// contract reference; spec §4.4).
    pub fn is_present(&self) -> bool {
        self.compare_status.is_some()
            || self.compare_body.is_some()
            || self.compare_headers.is_some()
            || self.json.as_ref().is_some_and(|j| !j.is_empty())
    }

    /// Collect every JSONPath string this layer references, for subset
    /// validation. Returns `(field_label, path)` pairs.
    pub fn json_paths(&self) -> Vec<(&'static str, &str)> {
        self.json.as_ref().map(JsonRules::paths).unwrap_or_default()
    }
}

/// JSON normalization rules. All lists default empty and *merge by
/// concatenation* across layers (spec §4.2: per-route paths are "merged with
/// defaults").
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JsonRules {
    /// Paths removed entirely before hashing/diffing.
    #[serde(default)]
    pub ignore_paths: Vec<String>,
    /// Paths masked in any output (logs, diffs).
    #[serde(default)]
    pub redact_paths: Vec<String>,
    /// Arrays sorted by a stable element key before comparison.
    #[serde(default)]
    pub sort_arrays: Vec<SortArray>,
    /// Arrays compared as unordered sets.
    #[serde(default)]
    pub unordered_arrays: Vec<UnorderedArray>,
    /// Timestamp fields normalized to a coarser precision.
    #[serde(default)]
    pub normalize_timestamps: Vec<NormalizeTimestamp>,
    /// Enum values mapped to a canonical spelling.
    #[serde(default)]
    pub enum_aliases: Vec<EnumAlias>,
}

impl JsonRules {
    /// Whether no rule is set.
    pub fn is_empty(&self) -> bool {
        self.ignore_paths.is_empty()
            && self.redact_paths.is_empty()
            && self.sort_arrays.is_empty()
            && self.unordered_arrays.is_empty()
            && self.normalize_timestamps.is_empty()
            && self.enum_aliases.is_empty()
    }

    /// Every JSONPath string referenced here, labeled by its source field.
    pub fn paths(&self) -> Vec<(&'static str, &str)> {
        let mut out: Vec<(&'static str, &str)> = Vec::new();
        out.extend(
            self.ignore_paths
                .iter()
                .map(|p| ("ignore_paths", p.as_str())),
        );
        out.extend(
            self.redact_paths
                .iter()
                .map(|p| ("redact_paths", p.as_str())),
        );
        out.extend(
            self.sort_arrays
                .iter()
                .map(|s| ("sort_arrays", s.path.as_str())),
        );
        out.extend(
            self.unordered_arrays
                .iter()
                .map(|u| ("unordered_arrays", u.path.as_str())),
        );
        out.extend(
            self.normalize_timestamps
                .iter()
                .map(|n| ("normalize_timestamps", n.path.as_str())),
        );
        out.extend(
            self.enum_aliases
                .iter()
                .map(|e| ("enum_aliases", e.path.as_str())),
        );
        out
    }
}

/// Sort an array (selected by `path`) by the value at element key `key`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SortArray {
    /// JSONPath to the array.
    pub path: String,
    /// Element object key to sort by.
    pub key: String,
}

/// Treat the array selected by `path` as an unordered set.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UnorderedArray {
    /// JSONPath to the array.
    pub path: String,
}

/// Normalize the timestamp at `path` to `precision`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizeTimestamp {
    /// JSONPath to the timestamp value.
    pub path: String,
    /// Coarsest precision both implementations can satisfy.
    pub precision: TimestampPrecision,
}

/// Timestamp precision for [`NormalizeTimestamp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TimestampPrecision {
    /// Truncate to whole seconds.
    Seconds,
    /// Truncate to whole milliseconds.
    Millis,
    /// Truncate to whole minutes.
    Minutes,
    /// Truncate to whole hours.
    Hours,
    /// Truncate to whole days.
    Days,
}

/// Map equivalent enum spellings to a canonical value at `path`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnumAlias {
    /// JSONPath to the enum value.
    pub path: String,
    /// `from -> to` mapping applied to the value at `path`.
    pub aliases: BTreeMap<String, String>,
}

/// Behavioral rules with every field resolved to a concrete value — the form
/// the comparison engine consumes. Produced by [`crate::contract::merge`].
#[derive(Debug, Clone, PartialEq)]
pub struct ComparisonRules {
    /// Whether to compare the HTTP status code.
    pub compare_status: bool,
    /// Whether to compare the normalized body.
    pub compare_body: bool,
    /// Header names to compare (empty = none).
    pub compare_headers: Vec<String>,
    /// Resolved JSON normalization rules.
    pub json: JsonRules,
}

impl Default for ComparisonRules {
    fn default() -> Self {
        // Safe defaults (spec §4.2): compare status and body, no headers.
        Self {
            compare_status: true,
            compare_body: true,
            compare_headers: Vec::new(),
            json: JsonRules::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn behavioral_rules_presence() {
        assert!(!BehavioralRules::default().is_present());
        assert!(BehavioralRules {
            compare_status: Some(false),
            ..Default::default()
        }
        .is_present());
        // An empty `json: {}` block does not count as a present rule.
        assert!(!BehavioralRules {
            json: Some(JsonRules::default()),
            ..Default::default()
        }
        .is_present());
        assert!(BehavioralRules {
            json: Some(JsonRules {
                ignore_paths: vec!["$.a".into()],
                ..Default::default()
            }),
            ..Default::default()
        }
        .is_present());
    }

    #[test]
    fn json_rules_paths_are_collected_with_labels() {
        let rules = JsonRules {
            ignore_paths: vec!["$.a".into()],
            sort_arrays: vec![SortArray {
                path: "$.items".into(),
                key: "id".into(),
            }],
            enum_aliases: vec![EnumAlias {
                path: "$.status".into(),
                aliases: BTreeMap::new(),
            }],
            ..Default::default()
        };
        let paths = rules.paths();
        assert!(paths.contains(&("ignore_paths", "$.a")));
        assert!(paths.contains(&("sort_arrays", "$.items")));
        assert!(paths.contains(&("enum_aliases", "$.status")));
    }

    #[test]
    fn deserializes_contract_yaml() {
        let yaml = r#"
version: 1
service: device-service
defaults:
  compare_status: true
  json:
    ignore_paths:
      - "$.metadata.requestId"
    enum_aliases:
      - path: "$.status"
        aliases: { ACTIVE: enabled, INACTIVE: disabled }
routes:
  - id: "get-device"
    match:
      methods: ["GET"]
      path_template: "/devices/{id}"
    comparison:
      json:
        ignore_paths: ["$.device.lastSeenAt"]
    tags: [read, migration-ready]
"#;
        let c: Contract = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(c.version, 1);
        assert_eq!(c.service, "device-service");
        let r = c.route("get-device").unwrap();
        assert_eq!(
            r.comparison
                .as_ref()
                .unwrap()
                .json
                .as_ref()
                .unwrap()
                .ignore_paths,
            vec!["$.device.lastSeenAt"]
        );
        assert_eq!(
            c.defaults.json.as_ref().unwrap().enum_aliases[0].aliases["ACTIVE"],
            "enabled"
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        let yaml = r#"
version: 1
service: s
defaults:
  compaer_status: true
"#;
        // A typo'd key is rejected rather than silently ignored.
        assert!(serde_yaml::from_str::<Contract>(yaml).is_err());
    }
}
