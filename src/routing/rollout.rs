//! Deterministic rollout hashing, assignment-key extraction, and the resolution
//! of a route's current rollout percentage (spec §6.4, §8.3).
//!
//! A request is assigned to legacy or new by hashing `route_id + ':' +
//! assignment_key` into a bucket `0..10000`; if the bucket is below
//! `percentage * 100`, new is chosen. Hashing the route id with the key keeps a
//! given tenant/user stable for a route while distributing independently across
//! routes. `blake3` gives a fast, stable hash.
//!
//! [`resolve_percentage`] is the *one* place the flag → percentage chain lives.
//! Two consumers read it — the router, which turns it into an upstream, and the
//! `/metrics` handler, which turns it into the target-percentage gauge — and
//! they must never be able to disagree: a gauge that reported a rollout the
//! router was not performing would be believed by exactly the review it would
//! mislead.

use axum::http::HeaderMap;

use crate::config::model::{AssignmentFallback, FailSafeMode, RolloutConfig};
use crate::flags::FlagProvider;

/// The number of buckets (0..`BUCKETS`).
const BUCKETS: u32 = 10_000;

/// What a route's rollout resolves to right now: a percentage of traffic
/// targeted at new, or the fail-safe mode that displaced it.
///
/// The fail-safe arm carries the *mode* rather than collapsing to a number, so
/// every consumer has to match it exhaustively and say what a new mode means
/// for it. Collapsing here would make a future `new_only` fail-safe silently
/// route to legacy at each site that forgot to look.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResolvedPercentage {
    /// The rollout target, clamped to `0..=100`.
    Percentage(f64),
    /// Flags are stale, so `fail_safe_mode` applies instead of any percentage.
    FailSafe(FailSafeMode),
}

/// Resolve a route's rollout percentage from the current flag state.
///
/// Stale flags win outright (safety invariant 1 — the percentage is not
/// lowered, it is displaced). Otherwise the flag's numeric value, or
/// `default_percentage` when the flag is unset or of another shape, clamped
/// into `0..=100` so neither a fat-fingered flag nor a bad default can express
/// a target outside the range the bucket test is defined on.
pub async fn resolve_percentage(
    rollout: &RolloutConfig,
    flags: &dyn FlagProvider,
    fail_safe_mode: FailSafeMode,
) -> ResolvedPercentage {
    if flags.health().stale {
        return ResolvedPercentage::FailSafe(fail_safe_mode);
    }
    ResolvedPercentage::Percentage(
        flags
            .get(&rollout.percentage_flag)
            .await
            .and_then(|v| v.as_f64())
            .unwrap_or(rollout.default_percentage)
            .clamp(0.0, 100.0),
    )
}

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

    use crate::config::model::AssignmentKey;
    use crate::flags::{FlagProviderHealth, FlagValue};
    use async_trait::async_trait;
    use std::time::Duration;

    /// A provider whose staleness and value the test dictates.
    struct Fake {
        stale: bool,
        value: Option<FlagValue>,
    }

    #[async_trait]
    impl FlagProvider for Fake {
        async fn get(&self, _key: &str) -> Option<FlagValue> {
            self.value.clone()
        }
        fn health(&self) -> FlagProviderHealth {
            FlagProviderHealth {
                stale: self.stale,
                last_success_age_ms: Some(0),
                consecutive_failures: 0,
            }
        }
        async fn refresh(&self) {}
        fn refresh_interval(&self) -> Option<Duration> {
            None
        }
    }

    fn rollout(default_percentage: f64) -> RolloutConfig {
        RolloutConfig {
            percentage_flag: "f".to_string(),
            default_percentage,
            assignment_key: AssignmentKey {
                header: Some("x-tenant-id".to_string()),
                fallback: AssignmentFallback::RequestRandom,
            },
        }
    }

    async fn resolve(flags: Fake, default_percentage: f64) -> ResolvedPercentage {
        resolve_percentage(
            &rollout(default_percentage),
            &flags,
            FailSafeMode::LegacyOnly,
        )
        .await
    }

    #[tokio::test]
    async fn stale_flags_resolve_to_the_fail_safe_mode_whatever_the_value_says() {
        let stale = Fake {
            stale: true,
            value: Some(FlagValue::Number(100.0)),
        };
        assert_eq!(
            resolve(stale, 0.0).await,
            ResolvedPercentage::FailSafe(FailSafeMode::LegacyOnly),
            "staleness displaces the percentage rather than lowering it"
        );
    }

    #[tokio::test]
    async fn a_missing_or_non_numeric_flag_falls_back_to_the_default() {
        let missing = Fake {
            stale: false,
            value: None,
        };
        assert_eq!(
            resolve(missing, 12.5).await,
            ResolvedPercentage::Percentage(12.5)
        );
        // A flag of the wrong shape is no value at all, not a zero.
        let wrong_type = Fake {
            stale: false,
            value: Some(FlagValue::String("all of it".to_string())),
        };
        assert_eq!(
            resolve(wrong_type, 12.5).await,
            ResolvedPercentage::Percentage(12.5)
        );
    }

    #[tokio::test]
    async fn out_of_range_values_clamp_into_zero_to_one_hundred() {
        for (value, expected) in [(150.0, 100.0), (-5.0, 0.0), (42.0, 42.0)] {
            let flags = Fake {
                stale: false,
                value: Some(FlagValue::Number(value)),
            };
            assert_eq!(
                resolve(flags, 0.0).await,
                ResolvedPercentage::Percentage(expected),
                "{value} must clamp to {expected}"
            );
        }
        // The default is clamped on the same path — a bad config cannot smuggle
        // an out-of-range target past the flag check.
        let missing = Fake {
            stale: false,
            value: None,
        };
        assert_eq!(
            resolve(missing, 400.0).await,
            ResolvedPercentage::Percentage(100.0)
        );
    }

    /// The shape, not the value: every consumer matches `ResolvedPercentage`
    /// (and `FailSafeMode` inside it) exhaustively, so a new fail-safe mode is a
    /// compile error at each site that has to decide what it means — never a
    /// silent inheritance of legacy's behavior.
    #[test]
    fn the_resolution_is_matched_exhaustively() {
        let described = |resolved: ResolvedPercentage| match resolved {
            ResolvedPercentage::Percentage(p) => p,
            ResolvedPercentage::FailSafe(FailSafeMode::LegacyOnly) => 0.0,
        };
        assert_eq!(described(ResolvedPercentage::Percentage(7.0)), 7.0);
        assert_eq!(
            described(ResolvedPercentage::FailSafe(FailSafeMode::LegacyOnly)),
            0.0
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
