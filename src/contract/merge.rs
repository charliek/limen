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
//! - **Optional blocks** (`set_cookie`, `location`): absent from both layers
//!   resolves to `None` (the dimension is not compared); present in either
//!   resolves field-wise by the two rules above, so a route can turn a
//!   default-declared dimension off (`compare: false`) or extend its lists.

use crate::contract::model::{
    BehavioralRules, ComparisonRules, CookieValueMode, EnumAlias, JsonRules, LocationRules,
    NormalizeTimestamp, OriginMode, ResolvedLocationRules, ResolvedSetCookieRules, SetCookieRules,
    SortArray, UnorderedArray,
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

/// Merge an optional block (`set_cookie`, `location`). Declared by neither
/// layer means no layer asked for the dimension, so it stays unresolved (not
/// compared); otherwise the absent layer contributes its empty defaults and
/// `resolve_fields` applies the scalar/list rules field by field.
fn merge_optional<L: Default, R>(
    base: Option<&L>,
    over: Option<&L>,
    resolve_fields: impl FnOnce(&L, &L) -> R,
) -> Option<R> {
    if base.is_none() && over.is_none() {
        return None;
    }
    let empty = L::default();
    Some(resolve_fields(
        base.unwrap_or(&empty),
        over.unwrap_or(&empty),
    ))
}

/// Merge two `set_cookie` layers.
fn merge_set_cookie(
    base: Option<&SetCookieRules>,
    over: Option<&SetCookieRules>,
) -> Option<ResolvedSetCookieRules> {
    merge_optional(base, over, |base, over| ResolvedSetCookieRules {
        compare: over.compare.or(base.compare).unwrap_or(true),
        ignore_cookies: concat_dedup::<String>(&base.ignore_cookies, &over.ignore_cookies),
        ignore_attributes: concat_dedup::<String>(&base.ignore_attributes, &over.ignore_attributes),
        compare_values: over
            .compare_values
            .or(base.compare_values)
            .unwrap_or(CookieValueMode::Exact),
    })
}

/// Merge two `location` layers.
fn merge_location(
    base: Option<&LocationRules>,
    over: Option<&LocationRules>,
) -> Option<ResolvedLocationRules> {
    merge_optional(base, over, |base, over| ResolvedLocationRules {
        compare: over.compare.or(base.compare).unwrap_or(true),
        ignore_query_params: concat_dedup::<String>(
            &base.ignore_query_params,
            &over.ignore_query_params,
        ),
        origin: over.origin.or(base.origin).unwrap_or(OriginMode::Exact),
    })
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
        set_cookie: merge_set_cookie(defaults.set_cookie.as_ref(), over.set_cookie.as_ref()),
        location: merge_location(defaults.location.as_ref(), over.location.as_ref()),
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

    fn rules_with_set_cookie(set_cookie: SetCookieRules) -> BehavioralRules {
        BehavioralRules {
            set_cookie: Some(set_cookie),
            ..Default::default()
        }
    }

    fn rules_with_location(location: LocationRules) -> BehavioralRules {
        BehavioralRules {
            location: Some(location),
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

    #[test]
    fn optional_blocks_stay_unresolved_when_no_layer_declares_them() {
        let merged = resolve(&BehavioralRules::default(), &BehavioralRules::default());
        assert!(merged.set_cookie.is_none());
        assert!(merged.location.is_none());
    }

    #[test]
    fn empty_block_resolves_to_the_documented_defaults() {
        let over = BehavioralRules {
            set_cookie: Some(SetCookieRules::default()),
            location: Some(LocationRules::default()),
            ..Default::default()
        };
        let merged = resolve(&BehavioralRules::default(), &over);
        assert_eq!(
            merged.set_cookie.unwrap(),
            ResolvedSetCookieRules {
                compare: true,
                ignore_cookies: vec![],
                ignore_attributes: vec![],
                compare_values: CookieValueMode::Exact,
            }
        );
        assert_eq!(
            merged.location.unwrap(),
            ResolvedLocationRules {
                compare: true,
                ignore_query_params: vec![],
                origin: OriginMode::Exact,
            }
        );
    }

    #[test]
    fn set_cookie_lists_concat_and_dedup_scalars_override() {
        // The decision-table case: the same `ignore_cookies` entry in both
        // layers resolves to a single occurrence.
        let defaults = rules_with_set_cookie(SetCookieRules {
            compare: Some(true),
            ignore_cookies: vec!["csrf_token".into()],
            ignore_attributes: vec!["Expires".into()],
            compare_values: Some(CookieValueMode::Exact),
        });
        let over = rules_with_set_cookie(SetCookieRules {
            ignore_cookies: vec!["csrf_token".into(), "session_hint".into()],
            compare_values: Some(CookieValueMode::Presence),
            ..Default::default()
        });
        let merged = resolve(&defaults, &over).set_cookie.unwrap();
        assert_eq!(merged.ignore_cookies, vec!["csrf_token", "session_hint"]);
        assert_eq!(merged.ignore_attributes, vec!["Expires"]);
        assert_eq!(merged.compare_values, CookieValueMode::Presence);
        assert!(merged.compare);
    }

    #[test]
    fn route_can_switch_a_default_declared_dimension_off() {
        let defaults = rules_with_set_cookie(SetCookieRules {
            ignore_cookies: vec!["csrf_token".into()],
            ..Default::default()
        });
        let over = rules_with_set_cookie(SetCookieRules {
            compare: Some(false),
            ..Default::default()
        });
        let merged = resolve(&defaults, &over).set_cookie.unwrap();
        assert!(!merged.compare);
        // The switch is independent of the lists, which still merge.
        assert_eq!(merged.ignore_cookies, vec!["csrf_token"]);
    }

    #[test]
    fn location_merges_query_params_and_origin() {
        let defaults = rules_with_location(LocationRules {
            ignore_query_params: vec!["state".into()],
            ..Default::default()
        });
        let over = rules_with_location(LocationRules {
            ignore_query_params: vec!["state".into(), "nonce".into()],
            origin: Some(OriginMode::Ignore),
            ..Default::default()
        });
        let merged = resolve(&defaults, &over).location.unwrap();
        assert_eq!(merged.ignore_query_params, vec!["state", "nonce"]);
        assert_eq!(merged.origin, OriginMode::Ignore);
        assert!(merged.compare);
    }

    #[test]
    fn a_block_declared_only_in_defaults_reaches_every_route() {
        let defaults = rules_with_location(LocationRules {
            origin: Some(OriginMode::Ignore),
            ..Default::default()
        });
        let merged = resolve(&defaults, &BehavioralRules::default())
            .location
            .unwrap();
        assert_eq!(merged.origin, OriginMode::Ignore);
    }
}
