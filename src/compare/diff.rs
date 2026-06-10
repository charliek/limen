//! JSON-aware structural diff (spec §7.3): bounded in count and value length,
//! and redacted before anything is produced.
//!
//! The diff runs over already-normalized values, so it never re-applies contract
//! rules. It is generated only after a hash mismatch, so an all-equal subtree is
//! never walked unnecessarily by callers.

use serde_json::Value;

use crate::compare::jsonpath::JsonPath;
use crate::compare::redact::{self, PathStep, REDACTED};
use crate::compare::result::{ChangeKind, Difference};

/// Bounds on diff output (spec §7.3): cap the number of differences and the
/// length of any single value rendered.
#[derive(Debug, Clone, Copy)]
pub struct DiffLimits {
    /// Maximum number of differences to record.
    pub max_differences: usize,
    /// Maximum serialized length of any single legacy/new value.
    pub max_value_len: usize,
}

impl Default for DiffLimits {
    fn default() -> Self {
        Self {
            max_differences: 100,
            max_value_len: 512,
        }
    }
}

/// Compute the structural differences between two normalized values. Returns the
/// (bounded, redacted) differences and whether the list was truncated.
pub fn diff(
    legacy: &Value,
    new: &Value,
    redact_paths: &[JsonPath],
    limits: &DiffLimits,
) -> (Vec<Difference>, bool) {
    let mut ctx = DiffCtx {
        out: Vec::new(),
        truncated: false,
        redact_paths,
        limits,
    };
    let mut location = Vec::new();
    ctx.walk(legacy, new, &mut location);
    (ctx.out, ctx.truncated)
}

struct DiffCtx<'a> {
    out: Vec<Difference>,
    truncated: bool,
    redact_paths: &'a [JsonPath],
    limits: &'a DiffLimits,
}

impl DiffCtx<'_> {
    /// Whether the difference budget is exhausted.
    fn full(&self) -> bool {
        self.out.len() >= self.limits.max_differences
    }

    fn walk(&mut self, legacy: &Value, new: &Value, location: &mut Vec<PathStep>) {
        if self.full() {
            self.truncated = true;
            return;
        }
        match (legacy, new) {
            (Value::Object(l), Value::Object(n)) => {
                // Union of keys, sorted for deterministic output.
                let mut keys: Vec<&String> = l.keys().chain(n.keys()).collect();
                keys.sort_unstable();
                keys.dedup();
                let children = keys
                    .into_iter()
                    .map(|key| (PathStep::Key(key.clone()), l.get(key), n.get(key)));
                self.walk_children(location, children);
            }
            (Value::Array(l), Value::Array(n)) => {
                let max = l.len().max(n.len());
                let children = (0..max).map(|i| (PathStep::Index(i), l.get(i), n.get(i)));
                self.walk_children(location, children);
            }
            (l, n) if l != n => self.emit(ChangeKind::Changed, Some(l), Some(n), location),
            _ => {}
        }
    }

    /// Walk a sequence of child slots (object keys or array indices), recursing
    /// into present-on-both pairs and emitting added/removed for the rest.
    fn walk_children<'b>(
        &mut self,
        location: &mut Vec<PathStep>,
        children: impl Iterator<Item = (PathStep, Option<&'b Value>, Option<&'b Value>)>,
    ) {
        for (step, legacy, new) in children {
            if self.full() {
                self.truncated = true;
                return;
            }
            location.push(step);
            match (legacy, new) {
                (Some(lv), Some(nv)) => self.walk(lv, nv, location),
                (Some(lv), None) => self.emit(ChangeKind::Removed, Some(lv), None, location),
                (None, Some(nv)) => self.emit(ChangeKind::Added, None, Some(nv), location),
                (None, None) => {}
            }
            location.pop();
        }
    }

    fn emit(
        &mut self,
        kind: ChangeKind,
        legacy: Option<&Value>,
        new: Option<&Value>,
        location: &[PathStep],
    ) {
        if self.full() {
            self.truncated = true;
            return;
        }
        let redacted = redact::is_redacted(location, self.redact_paths);
        let render = |v: Option<&Value>| -> Option<Value> {
            v.map(|value| {
                if redacted {
                    Value::String(REDACTED.to_string())
                } else {
                    bound_value(value, self.limits.max_value_len)
                }
            })
        };
        // For a redacted location, render only the contract-defined prefix:
        // deeper steps come from the response (e.g. a secret object key).
        let path = if redacted {
            let len = redact::safe_render_len(location, self.redact_paths);
            redact::render_path(&location[..len])
        } else {
            redact::render_path(location)
        };
        self.out.push(Difference {
            path,
            kind,
            legacy: render(legacy),
            new: render(new),
        });
    }
}

/// Bound a single value's rendered size to ~`max_len` bytes: long strings are
/// truncated at a UTF-8 char boundary, and large composite values are elided to
/// a byte count.
fn bound_value(value: &Value, max_len: usize) -> Value {
    let serialized = value.to_string();
    if serialized.len() <= max_len {
        return value.clone();
    }
    match value {
        Value::String(text) => {
            let mut end = max_len.min(text.len());
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            Value::String(format!("{}…", &text[..end]))
        }
        _ => Value::String(format!("<{} bytes elided>", serialized.len())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::jsonpath;
    use crate::compare::result::ChangeKind;
    use serde_json::json;

    fn diff_default(l: &Value, n: &Value) -> Vec<Difference> {
        diff(l, n, &[], &DiffLimits::default()).0
    }

    #[test]
    fn detects_changed_added_removed() {
        let legacy = json!({"name": "A", "only_legacy": 1, "nested": {"x": 1}});
        let new = json!({"name": "B", "only_new": 2, "nested": {"x": 1}});
        let diffs = diff_default(&legacy, &new);
        assert!(diffs
            .iter()
            .any(|d| d.path == "$.name" && d.kind == ChangeKind::Changed));
        assert!(diffs
            .iter()
            .any(|d| d.path == "$.only_legacy" && d.kind == ChangeKind::Removed));
        assert!(diffs
            .iter()
            .any(|d| d.path == "$.only_new" && d.kind == ChangeKind::Added));
        // Equal nested subtree produces no difference.
        assert!(!diffs.iter().any(|d| d.path.starts_with("$.nested")));
    }

    #[test]
    fn array_element_differences_use_indices() {
        let legacy = json!({"items": [1, 2, 3]});
        let new = json!({"items": [1, 9, 3, 4]});
        let diffs = diff_default(&legacy, &new);
        assert!(diffs
            .iter()
            .any(|d| d.path == "$.items[1]" && d.kind == ChangeKind::Changed));
        assert!(diffs
            .iter()
            .any(|d| d.path == "$.items[3]" && d.kind == ChangeKind::Added));
    }

    #[test]
    fn respects_max_difference_count() {
        let legacy = json!({"a": 1, "b": 2, "c": 3, "d": 4});
        let new = json!({"a": 9, "b": 9, "c": 9, "d": 9});
        let limits = DiffLimits {
            max_differences: 2,
            max_value_len: 512,
        };
        let (diffs, truncated) = diff(&legacy, &new, &[], &limits);
        assert_eq!(diffs.len(), 2);
        assert!(truncated);
    }

    #[test]
    fn respects_max_value_length() {
        let legacy = json!({"big": "x".repeat(100)});
        let new = json!({"big": "y".repeat(100)});
        let limits = DiffLimits {
            max_differences: 10,
            max_value_len: 16,
        };
        let (diffs, _) = diff(&legacy, &new, &[], &limits);
        let rendered = diffs[0].new.as_ref().unwrap().as_str().unwrap();
        assert!(
            rendered.chars().count() <= 17,
            "value should be truncated: {rendered:?}"
        );
    }

    #[test]
    fn redacts_values_at_redact_paths() {
        let legacy = json!({"user": {"email": "a@x.com"}});
        let new = json!({"user": {"email": "b@x.com"}});
        let redact = vec![jsonpath::parse("$.user.email").unwrap()];
        let (diffs, _) = diff(&legacy, &new, &redact, &DiffLimits::default());
        let d = diffs.iter().find(|d| d.path == "$.user.email").unwrap();
        // The difference is reported (so the mismatch is visible) but the secret
        // values are masked.
        assert_eq!(d.legacy, Some(json!("<redacted>")));
        assert_eq!(d.new, Some(json!("<redacted>")));
        let serialized = serde_json::to_string(&diffs).unwrap();
        assert!(!serialized.contains("a@x.com"));
        assert!(!serialized.contains("b@x.com"));
    }

    #[test]
    fn removed_subtree_containing_a_redacted_field_does_not_leak() {
        // The whole `user` object (containing the redacted email) is removed.
        // The diff emits it at the ancestor `$.user`, which must be masked.
        let legacy = json!({"user": {"email": "a@secret.com", "name": "A"}});
        let new = json!({});
        let redact = vec![jsonpath::parse("$.user.email").unwrap()];
        let (diffs, _) = diff(&legacy, &new, &redact, &DiffLimits::default());
        let serialized = serde_json::to_string(&diffs).unwrap();
        assert!(
            !serialized.contains("a@secret.com"),
            "redacted secret leaked via a removed ancestor subtree: {serialized}"
        );
    }

    #[test]
    fn root_type_change_does_not_leak_redacted_descendants() {
        // The whole document changes type (object -> array), emitting the entire
        // legacy object at the root. With a redact path configured, the root must
        // be masked rather than dumped wholesale.
        let legacy = json!({"user": {"email": "a@secret.com"}});
        let new = json!([]);
        let redact = vec![jsonpath::parse("$.user.email").unwrap()];
        let (diffs, _) = diff(&legacy, &new, &redact, &DiffLimits::default());
        let serialized = serde_json::to_string(&diffs).unwrap();
        assert!(
            !serialized.contains("a@secret.com"),
            "redacted secret leaked via a root type change: {serialized}"
        );
    }

    #[test]
    fn redacted_path_does_not_expose_secret_object_keys() {
        // A subtree keyed by a secret token: the value is masked AND the path is
        // truncated to the redacted prefix so the key itself does not leak.
        let legacy = json!({"tokens": {"super-secret-key": 1}});
        let new = json!({"tokens": {"super-secret-key": 2}});
        let redact = vec![jsonpath::parse("$.tokens").unwrap()];
        let (diffs, _) = diff(&legacy, &new, &redact, &DiffLimits::default());
        let serialized = serde_json::to_string(&diffs).unwrap();
        assert!(
            !serialized.contains("super-secret-key"),
            "secret object key leaked via the diff path: {serialized}"
        );
    }
}
