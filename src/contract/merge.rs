//! Merging behavioral-rule layers into resolved [`ComparisonRules`].
//!
//! Two layers combine — service `defaults` and a per-route (or inline)
//! override — following spec §4.2/§4.4:
//!
//! - **Scalars** (`compare_status`, `compare_body`): the override wins if set,
//!   else the default, else the safe built-in (`true`).
//! - **Lists** (`compare_headers` and every `json` list): *concatenate* the
//!   default's entries followed by the override's, de-duplicated preserving
//!   first occurrence. This is the "merged with defaults" union the spec
//!   annotates — never a reconciliation, because the namespaces are additive.

use crate::contract::model::{
    BehavioralRules, ComparisonRules, EnumAlias, JsonRules, NormalizeTimestamp, SortArray,
    UnorderedArray,
};

/// Concatenate two slices, de-duplicating by value and preserving the order of
/// first occurrence.
fn concat_dedup<T: Clone + PartialEq>(a: &[T], b: &[T]) -> Vec<T> {
    let mut out: Vec<T> = Vec::with_capacity(a.len() + b.len());
    for item in a.iter().chain(b.iter()) {
        if !out.contains(item) {
            out.push(item.clone());
        }
    }
    out
}

/// Merge two `json` layers by concatenating every list field.
fn merge_json(base: &JsonRules, over: &JsonRules) -> JsonRules {
    JsonRules {
        ignore_paths: concat_dedup::<String>(&base.ignore_paths, &over.ignore_paths),
        redact_paths: concat_dedup::<String>(&base.redact_paths, &over.redact_paths),
        sort_arrays: concat_dedup::<SortArray>(&base.sort_arrays, &over.sort_arrays),
        unordered_arrays: concat_dedup::<UnorderedArray>(
            &base.unordered_arrays,
            &over.unordered_arrays,
        ),
        normalize_timestamps: concat_dedup::<NormalizeTimestamp>(
            &base.normalize_timestamps,
            &over.normalize_timestamps,
        ),
        enum_aliases: concat_dedup::<EnumAlias>(&base.enum_aliases, &over.enum_aliases),
    }
}

/// Resolve service `defaults` and a per-route/inline `override` layer into the
/// concrete rules the comparison engine consumes.
pub fn resolve(defaults: &BehavioralRules, over: &BehavioralRules) -> ComparisonRules {
    let empty = JsonRules::default();
    ComparisonRules {
        compare_status: over
            .compare_status
            .or(defaults.compare_status)
            .unwrap_or(true),
        compare_body: over.compare_body.or(defaults.compare_body).unwrap_or(true),
        compare_headers: concat_dedup::<String>(
            defaults.compare_headers.as_deref().unwrap_or(&[]),
            over.compare_headers.as_deref().unwrap_or(&[]),
        ),
        json: merge_json(
            defaults.json.as_ref().unwrap_or(&empty),
            over.json.as_ref().unwrap_or(&empty),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::model::SortArray;

    fn rules_with_ignore(paths: &[&str]) -> BehavioralRules {
        BehavioralRules {
            json: Some(JsonRules {
                ignore_paths: paths.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn scalar_override_wins_else_default_else_true() {
        let defaults = BehavioralRules {
            compare_status: Some(false),
            ..Default::default()
        };
        // Override unset -> falls through to the default (false).
        assert!(!resolve(&defaults, &BehavioralRules::default()).compare_status);
        // Override set -> wins.
        let over = BehavioralRules {
            compare_status: Some(true),
            ..Default::default()
        };
        assert!(resolve(&defaults, &over).compare_status);
        // Neither set -> safe built-in true.
        let r = resolve(&BehavioralRules::default(), &BehavioralRules::default());
        assert!(r.compare_status && r.compare_body);
    }

    #[test]
    fn ignore_paths_concatenate_and_dedup() {
        let defaults = rules_with_ignore(&["$.a", "$.b"]);
        let over = rules_with_ignore(&["$.b", "$.c"]);
        let merged = resolve(&defaults, &over);
        assert_eq!(merged.json.ignore_paths, vec!["$.a", "$.b", "$.c"]);
    }

    #[test]
    fn compare_headers_union() {
        let defaults = BehavioralRules {
            compare_headers: Some(vec!["content-type".into()]),
            ..Default::default()
        };
        let over = BehavioralRules {
            compare_headers: Some(vec!["location".into(), "content-type".into()]),
            ..Default::default()
        };
        assert_eq!(
            resolve(&defaults, &over).compare_headers,
            vec!["content-type", "location"]
        );
    }

    #[test]
    fn sort_arrays_merge_preserves_distinct_entries() {
        let defaults = BehavioralRules {
            json: Some(JsonRules {
                sort_arrays: vec![SortArray {
                    path: "$.devices".into(),
                    key: "id".into(),
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let merged = resolve(&defaults, &BehavioralRules::default());
        assert_eq!(merged.json.sort_arrays.len(), 1);
    }
}
