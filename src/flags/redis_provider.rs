//! Redis flag provider: polls a Redis key space under a prefix, keeping the last
//! known good values on a connection failure and tracking staleness (spec §8.2).
//!
//! Values are read as strings and parsed by shape (JSON number/bool, else a
//! string), mirroring the file provider. The cache + staleness machinery is the
//! shared [`CachedFlags`], which the file provider's tests exercise; live-Redis
//! behavior needs a running server (see the ignored integration test).
//!
//! Designed so a remote provider (LaunchDarkly-style) can later replace this
//! behind the same [`FlagProvider`] trait.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use redis::AsyncCommands;
use tracing::warn;

use crate::flags::provider::{CachedFlags, FlagProvider, FlagProviderHealth, FlagValue};

/// A provider backed by a Redis key space under `key_prefix`.
pub struct RedisProvider {
    client: redis::Client,
    key_prefix: String,
    interval: Duration,
    cache: CachedFlags,
}

impl RedisProvider {
    /// Open a client for `url`. The connection is established lazily on the
    /// first refresh, so construction does not block or fail on an unreachable
    /// server (the provider simply stays stale until a refresh succeeds).
    pub fn new(
        url: &str,
        key_prefix: String,
        interval: Duration,
        stale_ttl: Duration,
    ) -> Result<Self, redis::RedisError> {
        Ok(Self {
            client: redis::Client::open(url)?,
            key_prefix,
            interval,
            cache: CachedFlags::new(stale_ttl),
        })
    }

    /// Fetch all flags under the prefix. Note: uses `KEYS` for simplicity, which
    /// is fine for the small flag key spaces Limen expects; switching to `SCAN`
    /// for very large key spaces is a documented future change.
    async fn load(&self) -> Result<HashMap<String, FlagValue>, redis::RedisError> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        let pattern = format!("{}*", self.key_prefix);
        let keys: Vec<String> = conn.keys(&pattern).await?;

        let mut values = HashMap::with_capacity(keys.len());
        for key in &keys {
            let raw: Option<String> = conn.get(key).await?;
            if let Some(raw) = raw {
                let flag_key = key
                    .strip_prefix(&self.key_prefix)
                    .unwrap_or(key)
                    .to_string();
                values.insert(flag_key, parse_flag_value(&raw));
            }
        }
        Ok(values)
    }
}

/// Parse a raw Redis string into a [`FlagValue`] by shape: a JSON number/bool,
/// otherwise the raw string.
pub(crate) fn parse_flag_value(raw: &str) -> FlagValue {
    serde_json::from_str(raw).unwrap_or_else(|_| FlagValue::String(raw.to_string()))
}

#[async_trait]
impl FlagProvider for RedisProvider {
    async fn get(&self, key: &str) -> Option<FlagValue> {
        self.cache.get(key)
    }

    fn health(&self) -> FlagProviderHealth {
        self.cache.health()
    }

    async fn refresh(&self) {
        // Bound the whole load (connect + reads) so an unreachable or blackholed
        // Redis can never hang a refresh — or block startup, since `serve` awaits
        // the initial refresh. On timeout the provider keeps last known good and
        // goes stale (fail-safe to legacy). Capped so a large poll interval still
        // bounds startup.
        let budget = self.interval.min(Duration::from_secs(5));
        match tokio::time::timeout(budget, self.load()).await {
            Ok(Ok(values)) => self.cache.record_success(values),
            Ok(Err(error)) => {
                warn!(%error, "redis flag refresh failed; keeping last known good");
                self.cache.record_failure();
            }
            Err(_elapsed) => {
                warn!(
                    timeout_ms = budget.as_millis(),
                    "redis flag refresh timed out; keeping last known good"
                );
                self.cache.record_failure();
            }
        }
    }

    fn refresh_interval(&self) -> Option<Duration> {
        Some(self.interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_values_by_shape() {
        assert_eq!(parse_flag_value("50"), FlagValue::Number(50.0));
        assert_eq!(parse_flag_value("0.25"), FlagValue::Number(0.25));
        assert_eq!(parse_flag_value("true"), FlagValue::Bool(true));
        // A bare (non-JSON) string falls back to a string value.
        assert_eq!(
            parse_flag_value("legacy_only"),
            FlagValue::String("legacy_only".into())
        );
        // A JSON-quoted string is a string too.
        assert_eq!(
            parse_flag_value("\"enabled\""),
            FlagValue::String("enabled".into())
        );
    }

    #[test]
    fn construction_does_not_block_on_unreachable_server() {
        let provider = RedisProvider::new(
            "redis://127.0.0.1:6390",
            "limen:flags:".to_string(),
            Duration::from_millis(1000),
            Duration::from_secs(30),
        );
        assert!(provider.is_ok());
        // Never refreshed against a server -> stale (fail-safe).
        assert!(provider.unwrap().health().stale);
    }
}
