//! Binary entrypoint: parse the CLI, initialize logging, and dispatch.
//!
//! All real logic lives in the `limen` library so it can be tested without a
//! running process; this file stays deliberately thin.

use clap::Parser;
use limen::cli::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse first so `--help` / `--version` short-circuit before we touch the
    // logging subsystem or the filesystem.
    let cli = Cli::parse();
    limen::observability::logging::init();
    cli.run().await
}
