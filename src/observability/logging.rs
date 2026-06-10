//! `tracing` subscriber setup.
//!
//! The default is a human-readable formatter at `info` level, overridable via
//! the standard `RUST_LOG` environment variable. A JSON formatter for
//! production log aggregation is wired in during Phase 7.

use std::sync::Once;

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

static INIT: Once = Once::new();

/// Initialize the global tracing subscriber.
///
/// Idempotent: safe to call more than once (e.g. from multiple tests) — only
/// the first call installs a subscriber. The filter defaults to `info` and
/// honors `RUST_LOG`.
pub fn init() {
    INIT.call_once(|| {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer())
            .init();
    });
}
