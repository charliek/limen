//! Binary entrypoint: parse the CLI, initialize logging, and dispatch.
//!
//! All real logic lives in the `limen` library so it can be tested without a
//! running process; this file stays deliberately thin.

use std::process::ExitCode;

use clap::Parser;
use limen::cli::Cli;

#[tokio::main]
async fn main() -> ExitCode {
    // Parse first so `--help` / `--version` short-circuit before we touch the
    // logging subsystem or the filesystem.
    let cli = Cli::parse();
    limen::observability::logging::init();
    // Before anything builds a TLS config. `false` means a provider was already
    // installed — only aws-lc-rs is compiled in, so that is this one: accept it.
    if !limen::http::client::install_crypto_provider() {
        tracing::debug!("a rustls crypto provider was already installed; keeping it");
    }
    // `verdict` exits with its documented typed codes; everything else maps
    // success/error to 0/1 exactly as the previous anyhow-returning main did.
    match cli.run().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e:?}");
            ExitCode::FAILURE
        }
    }
}
