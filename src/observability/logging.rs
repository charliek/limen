//! `tracing` subscriber setup.
//!
//! The level defaults to `info` and honors the standard `RUST_LOG` variable. The
//! formatter is human-readable by default; set `LIMEN_LOG_FORMAT=json` for
//! line-delimited JSON suited to production log aggregation (spec §10.2).

use std::sync::Once;

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

static INIT: Once = Once::new();

/// Initialize the global tracing subscriber.
///
/// Idempotent: safe to call more than once (e.g. from multiple tests) — only
/// the first call installs a subscriber.
pub fn init() {
    INIT.call_once(|| {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        let registry = tracing_subscriber::registry().with(filter);
        if json_format() {
            registry.with(fmt::layer().json()).init();
        } else {
            registry.with(fmt::layer()).init();
        }
    });
}

/// Whether to emit JSON logs, per `LIMEN_LOG_FORMAT`.
fn json_format() -> bool {
    std::env::var("LIMEN_LOG_FORMAT").is_ok_and(|v| v.eq_ignore_ascii_case("json"))
}
