//! Static flag provider: values fixed in config, never stale (spec §8.2).

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;

use crate::flags::provider::{FlagProvider, FlagProviderHealth, FlagValue};

/// A provider whose values are fixed at construction.
pub struct StaticProvider {
    values: BTreeMap<String, FlagValue>,
}

impl StaticProvider {
    /// Create a provider from fixed values.
    pub fn new(values: BTreeMap<String, FlagValue>) -> Self {
        Self { values }
    }
}

#[async_trait]
impl FlagProvider for StaticProvider {
    async fn get(&self, key: &str) -> Option<FlagValue> {
        self.values.get(key).cloned()
    }

    fn health(&self) -> FlagProviderHealth {
        // Static values are always fresh.
        FlagProviderHealth {
            stale: false,
            last_success_age_ms: Some(0),
            consecutive_failures: 0,
        }
    }

    async fn refresh(&self) {}

    fn refresh_interval(&self) -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_configured_values_and_is_never_stale() {
        let mut values = BTreeMap::new();
        values.insert("rollout".to_string(), FlagValue::Number(25.0));
        let provider = StaticProvider::new(values);
        assert_eq!(provider.get("rollout").await, Some(FlagValue::Number(25.0)));
        assert_eq!(provider.get("missing").await, None);
        assert!(!provider.health().stale);
    }
}
