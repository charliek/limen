//! `blake3` hashing over the normalized representation (spec §7.1).
//!
//! Two responses are equal when their normalized canonical forms hash equal.
//! Hashing is the fast first pass: only on a hash mismatch is a structural diff
//! generated.

use serde_json::Value;

use crate::compare::normalize::canonical_string;

/// A 32-byte BLAKE3 digest of a normalized JSON value.
pub type Digest = [u8; 32];

/// Hash a JSON value via its canonical (sorted-key) string form.
pub fn hash_value(value: &Value) -> Digest {
    *blake3::hash(canonical_string(value).as_bytes()).as_bytes()
}

/// Hash raw bytes (used for non-JSON bodies).
pub fn hash_bytes(bytes: &[u8]) -> Digest {
    *blake3::hash(bytes).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::normalize::normalize;
    use crate::contract::model::JsonRules;
    use serde_json::json;

    #[test]
    fn equal_values_hash_equal_regardless_of_key_order() {
        let a = json!({"a": 1, "b": 2});
        let b = json!({"b": 2, "a": 1});
        assert_eq!(hash_value(&a), hash_value(&b));
    }

    #[test]
    fn different_values_hash_differently() {
        assert_ne!(hash_value(&json!({"a": 1})), hash_value(&json!({"a": 2})));
    }

    #[test]
    fn ignored_fields_do_not_change_the_hash() {
        let rules = JsonRules {
            ignore_paths: vec!["$.ts".into()],
            ..Default::default()
        };
        let a = normalize(&json!({"v": 1, "ts": 100}), &rules);
        let b = normalize(&json!({"v": 1, "ts": 999}), &rules);
        assert_eq!(hash_value(&a), hash_value(&b));
    }
}
