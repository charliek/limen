//! The flag provider trait, flag value type, and the shared cache that file and
//! Redis providers poll into (spec §8).

use std::collections::HashMap;
use std::sync::{PoisonError, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A feature-flag value. Flags are loosely typed across providers (a rollout
/// percentage is a number, a `shadow_enabled` switch is a bool), so the value
/// is a small tagged-by-shape union.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum FlagValue {
    /// A boolean flag.
    Bool(bool),
    /// A numeric flag (e.g. a rollout percentage).
    Number(f64),
    /// A string flag.
    String(String),
}

impl FlagValue {
    /// The value as a number, if it is one.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            FlagValue::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// The value as a boolean, if it is one.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            FlagValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The value as a string slice, if it is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            FlagValue::String(s) => Some(s),
            _ => None,
        }
    }
}

/// A provider's health, including staleness (spec §8.3, §10.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FlagProviderHealth {
    /// Whether the values are stale beyond the configured TTL (apply fail-safe).
    pub stale: bool,
    /// Age of the last successful refresh, in milliseconds (`None` = never).
    pub last_success_age_ms: Option<u64>,
    /// Consecutive failed refreshes since the last success.
    pub consecutive_failures: u64,
}

/// A swappable source of feature-flag values. Providers cache values and refresh
/// them out of band; `get` reads the current cache. Implementations keep the
/// **last known good** values on a failed refresh and never panic.
#[async_trait]
pub trait FlagProvider: Send + Sync {
    /// The current value for a flag key, or `None` if unset.
    async fn get(&self, key: &str) -> Option<FlagValue>;

    /// Provider health, including staleness.
    fn health(&self) -> FlagProviderHealth;

    /// Perform one refresh cycle. A no-op for the static provider.
    async fn refresh(&self);

    /// The polling interval, or `None` if the provider never refreshes.
    fn refresh_interval(&self) -> Option<Duration>;
}

/// A thread-safe cache of flag values plus refresh bookkeeping, shared by the
/// file and Redis providers. Keeps the last known good values when a refresh
/// fails and tracks staleness against a TTL.
pub struct CachedFlags {
    inner: RwLock<Cached>,
    stale_ttl: Duration,
}

struct Cached {
    values: HashMap<String, FlagValue>,
    last_success: Option<Instant>,
    consecutive_failures: u64,
}

impl CachedFlags {
    /// Create an empty cache with the given staleness TTL.
    pub fn new(stale_ttl: Duration) -> Self {
        Self {
            inner: RwLock::new(Cached {
                values: HashMap::new(),
                last_success: None,
                consecutive_failures: 0,
            }),
            stale_ttl,
        }
    }

    /// Read a flag value.
    pub fn get(&self, key: &str) -> Option<FlagValue> {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values
            .get(key)
            .cloned()
    }

    /// Record a successful refresh: replace the values and reset failure state.
    pub fn record_success(&self, values: HashMap<String, FlagValue>) {
        let mut guard = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        guard.values = values;
        guard.last_success = Some(Instant::now());
        guard.consecutive_failures = 0;
    }

    /// Record a failed refresh: keep the last known good values, bump the
    /// failure count. Staleness is derived from the last *success*, so repeated
    /// failures eventually make the provider stale.
    pub fn record_failure(&self) {
        let mut guard = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        guard.consecutive_failures += 1;
    }

    /// Current health, computing staleness from the last successful refresh.
    pub fn health(&self) -> FlagProviderHealth {
        let guard = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let age = guard.last_success.map(|t| t.elapsed());
        // Never-succeeded or older than the TTL counts as stale.
        let stale = match age {
            Some(elapsed) => elapsed > self.stale_ttl,
            None => true,
        };
        FlagProviderHealth {
            stale,
            last_success_age_ms: age.map(|e| e.as_millis() as u64),
            consecutive_failures: guard.consecutive_failures,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untagged_parsing_picks_the_right_shape() {
        assert_eq!(
            serde_yaml::from_str::<FlagValue>("true").unwrap(),
            FlagValue::Bool(true)
        );
        assert_eq!(
            serde_yaml::from_str::<FlagValue>("25").unwrap(),
            FlagValue::Number(25.0)
        );
        assert_eq!(
            serde_yaml::from_str::<FlagValue>("0.5").unwrap(),
            FlagValue::Number(0.5)
        );
        assert_eq!(
            serde_yaml::from_str::<FlagValue>("\"legacy_only\"").unwrap(),
            FlagValue::String("legacy_only".into())
        );
    }

    #[test]
    fn typed_accessors() {
        assert_eq!(FlagValue::Number(5.0).as_f64(), Some(5.0));
        assert_eq!(FlagValue::Number(5.0).as_bool(), None);
        assert_eq!(FlagValue::Bool(true).as_bool(), Some(true));
        assert_eq!(FlagValue::String("x".into()).as_str(), Some("x"));
    }

    #[test]
    fn cache_keeps_last_known_good_and_tracks_staleness() {
        let cache = CachedFlags::new(Duration::from_secs(30));
        // Never refreshed -> stale, no values.
        assert!(cache.health().stale);
        assert_eq!(cache.get("k"), None);

        let mut values = HashMap::new();
        values.insert("k".to_string(), FlagValue::Number(50.0));
        cache.record_success(values);
        assert_eq!(cache.get("k"), Some(FlagValue::Number(50.0)));
        assert!(!cache.health().stale);

        // A failed refresh keeps the last good value, but counts the failure.
        cache.record_failure();
        assert_eq!(cache.get("k"), Some(FlagValue::Number(50.0)));
        assert_eq!(cache.health().consecutive_failures, 1);
    }

    #[test]
    fn cache_goes_stale_after_ttl() {
        let cache = CachedFlags::new(Duration::from_millis(1));
        cache.record_success(HashMap::new());
        std::thread::sleep(Duration::from_millis(10));
        assert!(
            cache.health().stale,
            "should be stale after the TTL elapses"
        );
    }
}
