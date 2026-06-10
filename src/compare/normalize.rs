//! JSON normalization (spec §7.2): apply the merged contract rules to a JSON
//! value before hashing and diffing, so incidental differences disappear and
//! only meaningful ones remain.
//!
//! Transforms applied, in order:
//! 1. **ignore_paths** — remove the matched fields entirely.
//! 2. **enum_aliases** — map equivalent enum spellings to a canonical value.
//! 3. **normalize_timestamps** — truncate timestamps to a coarser precision.
//! 4. **sort_arrays** — order an array by a stable element key.
//! 5. **unordered_arrays** — order an array as a set (by canonical element form).
//!
//! Object key order is handled at serialization time by [`canonical_string`],
//! which sorts keys deterministically regardless of the `serde_json` map
//! backing — so key order never causes a false mismatch.
//!
//! Redaction is deliberately *not* applied here: the hash and structural diff
//! run over real values (so a difference in a sensitive field is still
//! detected), and values are masked only when a diff is rendered
//! ([`crate::compare::redact`]).

use serde_json::Value;

use crate::compare::jsonpath::{self, Segment};
use crate::contract::model::{JsonRules, TimestampPrecision};

/// Apply the contract's JSON rules to a value, returning the normalized form.
pub fn normalize(value: &Value, rules: &JsonRules) -> Value {
    let mut v = value.clone();

    for path in &rules.ignore_paths {
        if let Ok(p) = jsonpath::parse(path) {
            remove_at_path(&mut v, p.segments());
        }
    }
    for alias in &rules.enum_aliases {
        for_each_match(&mut v, &alias.path, &mut |node| {
            if let Value::String(s) = node {
                if let Some(canonical) = alias.aliases.get(s) {
                    *node = Value::String(canonical.clone());
                }
            }
        });
    }
    for ts in &rules.normalize_timestamps {
        for_each_match(&mut v, &ts.path, &mut |node| {
            normalize_timestamp(node, ts.precision);
        });
    }
    for sort in &rules.sort_arrays {
        let key = sort.key.as_str();
        for_each_match(&mut v, &sort.path, &mut |node| {
            if let Value::Array(arr) = node {
                // Sort by the element key, tie-breaking on the full canonical
                // form so arrays with *duplicate* keys still normalize
                // deterministically (order-independent). `cached_key` computes
                // each element's expensive canonical form once.
                arr.sort_by_cached_key(|element| {
                    let element_key = element.get(key).map(canonical_string).unwrap_or_default();
                    (element_key, canonical_string(element))
                });
            }
        });
    }
    for unordered in &rules.unordered_arrays {
        for_each_match(&mut v, &unordered.path, &mut |node| {
            if let Value::Array(arr) = node {
                arr.sort_by_cached_key(canonical_string);
            }
        });
    }

    v
}

/// Parse a JSONPath and apply `f` to every matching node. A path that fails to
/// parse is a no-op (paths are validated at load time, so this is defensive).
fn for_each_match(value: &mut Value, path: &str, f: &mut impl FnMut(&mut Value)) {
    if let Ok(p) = jsonpath::parse(path) {
        apply_at_path(value, p.segments(), f);
    }
}

/// Apply `f` to every node matching the full path.
pub(crate) fn apply_at_path(
    value: &mut Value,
    segments: &[Segment],
    f: &mut impl FnMut(&mut Value),
) {
    match segments.split_first() {
        None => f(value),
        Some((Segment::Field(name), rest)) => {
            if let Value::Object(map) = value {
                if let Some(child) = map.get_mut(name) {
                    apply_at_path(child, rest, f);
                }
            }
        }
        Some((Segment::Wildcard, rest)) => {
            if let Value::Array(arr) = value {
                for child in arr.iter_mut() {
                    apply_at_path(child, rest, f);
                }
            }
        }
    }
}

/// Remove the field named by the final segment from every matching parent. The
/// supported JSONPath grammar guarantees the last segment is a field.
fn remove_at_path(value: &mut Value, segments: &[Segment]) {
    if let Some((Segment::Field(name), parents)) = segments.split_last() {
        apply_at_path(value, parents, &mut |parent| {
            if let Value::Object(map) = parent {
                map.remove(name);
            }
        });
    }
}

/// Truncate a string RFC3339 timestamp to `precision`, **converting to UTC**
/// first so equivalent instants in different timezones normalize equal (and,
/// crucially, different instants never collapse to the same value). Leaves
/// non-timestamp values (and unparseable strings) unchanged so normalization
/// never corrupts data.
fn normalize_timestamp(node: &mut Value, precision: TimestampPrecision) {
    if let Value::String(s) = node {
        if let Some(truncated) = truncate_rfc3339(s, precision) {
            *node = Value::String(truncated);
        }
    }
}

/// Parse an RFC3339 timestamp, convert it to UTC, and re-emit it truncated to
/// `precision` with a `Z` designator. Returns `None` if `s` is not RFC3339, so a
/// non-timestamp string is left untouched.
fn truncate_rfc3339(s: &str, precision: TimestampPrecision) -> Option<String> {
    use time::format_description::well_known::Rfc3339;
    use time::{OffsetDateTime, UtcOffset};

    let utc = OffsetDateTime::parse(s, &Rfc3339)
        .ok()?
        .to_offset(UtcOffset::UTC);
    let (y, mo, d) = (utc.year(), utc.month() as u8, utc.day());
    let (h, mi, sec) = (utc.hour(), utc.minute(), utc.second());

    let out = match precision {
        TimestampPrecision::Days => format!("{y:04}-{mo:02}-{d:02}T00:00:00Z"),
        TimestampPrecision::Hours => format!("{y:04}-{mo:02}-{d:02}T{h:02}:00:00Z"),
        TimestampPrecision::Minutes => format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:00Z"),
        TimestampPrecision::Seconds => {
            format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{sec:02}Z")
        }
        TimestampPrecision::Millis => {
            format!(
                "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{sec:02}.{:03}Z",
                utc.millisecond()
            )
        }
    };
    Some(out)
}

/// Canonical JSON string with object keys sorted recursively — the form that is
/// hashed. Deterministic regardless of the `serde_json` map ordering.
pub fn canonical_string(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push(':');
                write_canonical(&map[*k], out);
            }
            out.push('}');
        }
        Value::Array(arr) => {
            out.push('[');
            for (i, v) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(v, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&serde_json::to_string(scalar).unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::model::{EnumAlias, NormalizeTimestamp, SortArray, UnorderedArray};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn rules() -> JsonRules {
        JsonRules::default()
    }

    #[test]
    fn canonical_string_is_key_order_independent() {
        let a = json!({"b": 1, "a": 2, "nested": {"y": 1, "x": 2}});
        let b = json!({"a": 2, "nested": {"x": 2, "y": 1}, "b": 1});
        assert_eq!(canonical_string(&a), canonical_string(&b));
    }

    #[test]
    fn ignore_paths_remove_fields() {
        let mut r = rules();
        r.ignore_paths = vec!["$.metadata.requestId".into(), "$.items[*].ts".into()];
        let v = json!({
            "metadata": {"requestId": "x", "kept": 1},
            "items": [{"id": 1, "ts": 100}, {"id": 2, "ts": 200}]
        });
        let n = normalize(&v, &r);
        assert_eq!(n["metadata"].get("requestId"), None);
        assert_eq!(n["metadata"]["kept"], json!(1));
        assert_eq!(n["items"][0].get("ts"), None);
        assert_eq!(n["items"][1]["id"], json!(2));
    }

    #[test]
    fn enum_aliases_map_values() {
        let mut r = rules();
        let mut aliases = BTreeMap::new();
        aliases.insert("ACTIVE".to_string(), "enabled".to_string());
        r.enum_aliases = vec![EnumAlias {
            path: "$.status".into(),
            aliases,
        }];
        let n = normalize(&json!({"status": "ACTIVE"}), &r);
        assert_eq!(n["status"], json!("enabled"));
        // Unmapped values are left alone.
        let n2 = normalize(&json!({"status": "PENDING"}), &r);
        assert_eq!(n2["status"], json!("PENDING"));
    }

    #[test]
    fn sort_arrays_by_key_makes_order_irrelevant() {
        let mut r = rules();
        r.sort_arrays = vec![SortArray {
            path: "$.devices".into(),
            key: "id".into(),
        }];
        let a = json!({"devices": [{"id": "b"}, {"id": "a"}]});
        let b = json!({"devices": [{"id": "a"}, {"id": "b"}]});
        assert_eq!(
            canonical_string(&normalize(&a, &r)),
            canonical_string(&normalize(&b, &r))
        );
    }

    #[test]
    fn unordered_arrays_compare_as_sets() {
        let mut r = rules();
        r.unordered_arrays = vec![UnorderedArray {
            path: "$.permissions".into(),
        }];
        let a = json!({"permissions": ["read", "write", "admin"]});
        let b = json!({"permissions": ["admin", "read", "write"]});
        assert_eq!(
            canonical_string(&normalize(&a, &r)),
            canonical_string(&normalize(&b, &r))
        );
    }

    #[test]
    fn timestamp_precision_normalization() {
        let mut r = rules();
        r.normalize_timestamps = vec![NormalizeTimestamp {
            path: "$.createdAt".into(),
            precision: TimestampPrecision::Seconds,
        }];
        let millis = json!({"createdAt": "2024-01-01T12:30:45.123Z"});
        let offset = json!({"createdAt": "2024-01-01T12:30:45+00:00"});
        let n1 = normalize(&millis, &r);
        let n2 = normalize(&offset, &r);
        assert_eq!(n1["createdAt"], json!("2024-01-01T12:30:45Z"));
        assert_eq!(canonical_string(&n1), canonical_string(&n2));
    }

    #[test]
    fn timestamp_millis_precision_keeps_three_digits() {
        let mut r = rules();
        r.normalize_timestamps = vec![NormalizeTimestamp {
            path: "$.t".into(),
            precision: TimestampPrecision::Millis,
        }];
        let n = normalize(&json!({"t": "2024-01-01T00:00:00.123456Z"}), &r);
        assert_eq!(n["t"], json!("2024-01-01T00:00:00.123Z"));
    }

    #[test]
    fn timestamp_offset_is_converted_to_utc() {
        let mut r = rules();
        r.normalize_timestamps = vec![NormalizeTimestamp {
            path: "$.t".into(),
            precision: TimestampPrecision::Seconds,
        }];
        // +05:30 must convert to UTC (12:30:45 − 05:30 = 07:00:45), not be
        // relabeled `Z`.
        let n = normalize(&json!({"t": "2024-01-01T12:30:45+05:30"}), &r);
        assert_eq!(n["t"], json!("2024-01-01T07:00:45Z"));

        // Same wall clock with a different offset is a DIFFERENT instant and
        // must NOT collapse to the same value (no false match).
        let utc = normalize(&json!({"t": "2024-01-01T12:30:45Z"}), &r);
        assert_ne!(
            canonical_string(&n),
            canonical_string(&utc),
            "different instants must not normalize equal"
        );
    }

    #[test]
    fn non_timestamp_strings_are_untouched() {
        let mut r = rules();
        r.normalize_timestamps = vec![NormalizeTimestamp {
            path: "$.t".into(),
            precision: TimestampPrecision::Seconds,
        }];
        let n = normalize(&json!({"t": "not-a-timestamp"}), &r);
        assert_eq!(n["t"], json!("not-a-timestamp"));
    }
}
