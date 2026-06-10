//! Deterministic rollout hashing and assignment-key extraction (spec §6.4).
//!
//! A request is assigned to legacy or new by hashing `route_id + ':' +
//! assignment_key` into a bucket `0..10000`; if the bucket is below
//! `percentage * 100`, new is chosen. Hashing the route id with the key keeps a
//! given tenant/user stable for a route while distributing independently across
//! routes. `blake3` gives a fast, stable hash.

use axum::http::HeaderMap;

use crate::config::model::AssignmentFallback;

/// The number of buckets (0..`BUCKETS`).
const BUCKETS: u32 = 10_000;

/// Deterministically hash a route + assignment key into a bucket `0..10000`.
pub fn bucket(route_id: &str, assignment_key: &str) -> u32 {
    let digest = blake3::hash(format!("{route_id}:{assignment_key}").as_bytes());
    let b = digest.as_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]]) % BUCKETS
}

/// Whether a bucket selects the new upstream at the given rollout `percentage`
/// (0–100). `0` selects none; `100` selects all.
pub fn selects_new(bucket: u32, percentage: f64) -> bool {
    f64::from(bucket) < percentage * f64::from(BUCKETS) / 100.0
}

/// Derive the assignment key for a request: the configured header's value, or
/// the fallback when the header is absent. `request_random` yields a fresh
/// random key per request (so unkeyed requests distribute by the percentage).
pub fn assignment_key(
    header: Option<&str>,
    fallback: AssignmentFallback,
    headers: &HeaderMap,
) -> String {
    if let Some(name) = header {
        if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
            return value.to_string();
        }
    }
    match fallback {
        AssignmentFallback::RequestRandom => format!("{:016x}", rand::random::<u64>()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn bucket_is_deterministic_and_in_range() {
        let a = bucket("get-device", "tenant-1");
        let b = bucket("get-device", "tenant-1");
        assert_eq!(a, b, "same inputs hash to the same bucket");
        assert!(a < BUCKETS);
        // Different route id => independent assignment for the same key.
        assert_ne!(
            bucket("get-device", "tenant-1"),
            bucket("list-devices", "tenant-1")
        );
    }

    #[test]
    fn zero_and_hundred_percent_are_absolute() {
        for b in [0, 1, 4999, 5000, 9999] {
            assert!(!selects_new(b, 0.0), "0% selects nobody");
            assert!(selects_new(b, 100.0), "100% selects everybody");
        }
    }

    #[test]
    fn distribution_is_roughly_the_percentage() {
        let selected = (0..10_000)
            .filter(|i| selects_new(bucket("r", &format!("key-{i}")), 25.0))
            .count();
        // ~25% of 10k keys, allowing generous tolerance for hash variance.
        assert!(
            (2000..3000).contains(&selected),
            "expected ~2500 selected, got {selected}"
        );
    }

    #[test]
    fn assignment_key_prefers_header_then_falls_back() {
        let mut headers = HeaderMap::new();
        headers.insert("x-tenant-id", HeaderValue::from_static("t-42"));
        assert_eq!(
            assignment_key(
                Some("x-tenant-id"),
                AssignmentFallback::RequestRandom,
                &headers
            ),
            "t-42"
        );
        // Missing header => random fallback (two draws almost surely differ).
        let a = assignment_key(
            Some("x-missing"),
            AssignmentFallback::RequestRandom,
            &headers,
        );
        let b = assignment_key(
            Some("x-missing"),
            AssignmentFallback::RequestRandom,
            &headers,
        );
        assert_ne!(a, b);
    }
}
