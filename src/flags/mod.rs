//! Feature flags: the provider trait and its implementations.
//!
//! Flags sit behind a trait so providers are swappable (Section 8). All
//! providers keep the *last known good* value on a failed refresh, track
//! staleness, and never crash the proxy; beyond `stale_ttl_ms` the configured
//! fail-safe mode applies.
//!
//! Submodules:
//! - [`provider`] — the [`FlagProvider`] trait, [`FlagValue`], and the shared
//!   [`provider::CachedFlags`] cache.
//! - [`static_provider`], [`file_provider`], [`redis_provider`] — implementations.

pub mod file_provider;
pub mod provider;
pub mod redis_provider;
pub mod static_provider;

use std::sync::Arc;
use std::time::Duration;

pub use provider::{FlagProvider, FlagProviderHealth, FlagValue};

use crate::config::model::{FlagProviderKind, FlagsConfig};

/// Build the configured flag provider. File/Redis providers are returned ready
/// to poll; the caller spawns the refresh loop (see [`crate::http::server`]).
pub fn build(config: &FlagsConfig) -> anyhow::Result<Arc<dyn FlagProvider>> {
    let stale_ttl = Duration::from_millis(config.stale_ttl_ms);
    let provider: Arc<dyn FlagProvider> = match config.provider {
        FlagProviderKind::Static => Arc::new(static_provider::StaticProvider::new(
            config.static_values.values.clone(),
        )),
        FlagProviderKind::File => Arc::new(file_provider::FileProvider::new(
            config.file.path.clone(),
            Duration::from_millis(config.file.refresh_interval_ms),
            stale_ttl,
        )),
        FlagProviderKind::Redis => Arc::new(redis_provider::RedisProvider::new(
            &config.redis.url,
            config.redis.key_prefix.clone(),
            Duration::from_millis(config.redis.refresh_interval_ms),
            stale_ttl,
        )?),
    };
    Ok(provider)
}
