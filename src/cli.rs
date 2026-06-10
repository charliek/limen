//! Command-line interface: the `clap` subcommands and their dispatch.
//!
//! Four subcommands mirror the spec (Section 4.5, 14):
//! - `run` — bind the data-plane and control-plane listeners and serve.
//! - `validate-config` — semantically validate a configuration file.
//! - `print-routes` — print the resolved routing table for a configuration.
//! - `check-contract` — validate a behavioral contract and its JSONPath usage.
//!
//! The argument *structure* is stable from Phase 0; individual command bodies
//! are filled in as their phase lands. Each handler returns
//! [`anyhow::Result`] so the binary can surface a clear top-level error.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Limen — a migration proxy that safely shifts HTTP traffic from a legacy
/// service to a new implementation.
#[derive(Debug, Parser)]
#[command(name = "limen", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// The top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the proxy: bind the data-plane and control-plane listeners.
    Run(RunArgs),
    /// Semantically validate a configuration file and report field-level errors.
    ValidateConfig(ConfigArgs),
    /// Print the resolved routing table for a configuration.
    PrintRoutes(ConfigArgs),
    /// Validate a behavioral contract and its JSONPath-subset compliance.
    CheckContract(CheckContractArgs),
}

/// Arguments for `run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Path to the limen configuration file (YAML).
    #[arg(short, long, env = "LIMEN_CONFIG", default_value = "limen.config.yaml")]
    pub config: PathBuf,
}

/// Arguments for the config-oriented subcommands (`validate-config`,
/// `print-routes`).
#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// Path to the limen configuration file (YAML).
    #[arg(short, long, env = "LIMEN_CONFIG", default_value = "limen.config.yaml")]
    pub config: PathBuf,
}

/// Arguments for `check-contract`.
#[derive(Debug, Args)]
pub struct CheckContractArgs {
    /// Path to the contract file (YAML or JSON).
    pub contract: PathBuf,
}

impl Cli {
    /// Dispatch the parsed command.
    pub async fn run(self) -> anyhow::Result<()> {
        match self.command {
            Command::Run(args) => cmd_run(args).await,
            Command::ValidateConfig(args) => cmd_validate_config(args),
            Command::PrintRoutes(args) => cmd_print_routes(args),
            Command::CheckContract(args) => cmd_check_contract(args),
        }
    }
}

// The command bodies below are Phase-0 stubs that establish the wiring. They
// are replaced with real implementations in the phase that owns each feature
// (config/contract in Phase 1, serving in Phase 2).

async fn cmd_run(args: RunArgs) -> anyhow::Result<()> {
    anyhow::bail!(
        "`limen run` is not implemented yet (lands in Phase 2); requested config: {}",
        args.config.display()
    );
}

fn cmd_validate_config(args: ConfigArgs) -> anyhow::Result<()> {
    anyhow::bail!(
        "`limen validate-config` is not implemented yet (lands in Phase 1); requested config: {}",
        args.config.display()
    );
}

fn cmd_print_routes(args: ConfigArgs) -> anyhow::Result<()> {
    anyhow::bail!(
        "`limen print-routes` is not implemented yet (lands in Phase 1); requested config: {}",
        args.config.display()
    );
}

fn cmd_check_contract(args: CheckContractArgs) -> anyhow::Result<()> {
    anyhow::bail!(
        "`limen check-contract` is not implemented yet (lands in Phase 1); requested contract: {}",
        args.contract.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        // Catches structural mistakes (duplicate args, bad defaults) at test time.
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_each_subcommand() {
        assert!(matches!(
            Cli::parse_from(["limen", "run", "--config", "x.yaml"]).command,
            Command::Run(_)
        ));
        assert!(matches!(
            Cli::parse_from(["limen", "validate-config", "-c", "x.yaml"]).command,
            Command::ValidateConfig(_)
        ));
        assert!(matches!(
            Cli::parse_from(["limen", "print-routes"]).command,
            Command::PrintRoutes(_)
        ));
        assert!(matches!(
            Cli::parse_from(["limen", "check-contract", "c.yaml"]).command,
            Command::CheckContract(_)
        ));
    }
}
