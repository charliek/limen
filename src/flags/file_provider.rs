//! File flag provider: polls a YAML file, keeping the last known good values on
//! an invalid update and tracking staleness (spec §8.2, §8.3).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use tracing::warn;

use crate::flags::provider::{CachedFlags, FlagProvider, FlagProviderHealth, FlagValue};

/// A provider backed by a flat YAML `key: value` file.
pub struct FileProvider {
    path: PathBuf,
    interval: Duration,
    cache: CachedFlags,
}

impl FileProvider {
    /// Create the provider and perform an initial (best-effort) load so values
    /// are available before the first poll. A failed initial load leaves the
    /// provider stale (fail-safe) until a refresh succeeds.
    pub fn new(path: PathBuf, interval: Duration, stale_ttl: Duration) -> Self {
        let provider = Self {
            path,
            interval,
            cache: CachedFlags::new(stale_ttl),
        };
        provider.load_into_cache();
        provider
    }

    fn load_into_cache(&self) {
        match Self::load(&self.path) {
            Ok(values) => self.cache.record_success(values),
            Err(error) => {
                warn!(path = %self.path.display(), %error, "flag file refresh failed; keeping last known good");
                self.cache.record_failure();
            }
        }
    }

    /// Parse the flags file into a value map.
    fn load(path: &Path) -> Result<HashMap<String, FlagValue>, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        serde_yaml::from_str(&text).map_err(|e| e.to_string())
    }
}

#[async_trait]
impl FlagProvider for FileProvider {
    async fn get(&self, key: &str) -> Option<FlagValue> {
        self.cache.get(key)
    }

    fn health(&self) -> FlagProviderHealth {
        self.cache.health()
    }

    async fn refresh(&self) {
        self.load_into_cache();
    }

    fn refresh_interval(&self) -> Option<Duration> {
        Some(self.interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
    }

    #[tokio::test]
    async fn loads_reloads_and_keeps_last_known_good() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flags.yaml");
        write(&path, "migration.rollout: 0\nshadow_enabled: true\n");

        let provider = FileProvider::new(
            path.clone(),
            Duration::from_millis(1000),
            Duration::from_secs(30),
        );
        assert_eq!(
            provider.get("migration.rollout").await,
            Some(FlagValue::Number(0.0))
        );
        assert_eq!(
            provider.get("shadow_enabled").await,
            Some(FlagValue::Bool(true))
        );
        assert!(!provider.health().stale);

        // A valid update is picked up on refresh — without restart.
        write(&path, "migration.rollout: 100\n");
        provider.refresh().await;
        assert_eq!(
            provider.get("migration.rollout").await,
            Some(FlagValue::Number(100.0))
        );

        // An invalid update keeps the last known good values.
        write(&path, "this: : not: valid: yaml:\n");
        provider.refresh().await;
        assert_eq!(
            provider.get("migration.rollout").await,
            Some(FlagValue::Number(100.0)),
            "invalid update must keep the last known good value",
        );
        assert!(provider.health().consecutive_failures >= 1);
    }

    #[tokio::test]
    async fn missing_file_is_stale_not_a_panic() {
        let provider = FileProvider::new(
            PathBuf::from("/nonexistent/flags.yaml"),
            Duration::from_millis(1000),
            Duration::from_secs(30),
        );
        assert_eq!(provider.get("anything").await, None);
        assert!(provider.health().stale);
    }
}
