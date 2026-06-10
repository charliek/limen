//! Command-line interface: the `clap` subcommands and their dispatch.
//!
//! Four subcommands mirror the spec (Section 4.5, 14):
//! - `run` — bind the data-plane and control-plane listeners and serve.
//! - `validate-config` — semantically validate a configuration file.
//! - `print-routes` — print the resolved routing table for a configuration.
//! - `check-contract` — validate a behavioral contract and its JSONPath usage.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::config::{self, ConfigOverrides, LoadedConfig};
use crate::contract::load as contract_load;

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
    Run(ConfigArgs),
    /// Semantically validate a configuration file and report field-level errors.
    ValidateConfig(ConfigArgs),
    /// Print the resolved routing table for a configuration.
    PrintRoutes(ConfigArgs),
    /// Validate a behavioral contract and its JSONPath-subset compliance.
    CheckContract(CheckContractArgs),
}

/// Arguments shared by the config-oriented subcommands.
#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// Path to the limen configuration file (YAML).
    #[arg(short, long, env = "LIMEN_CONFIG", default_value = "limen.config.yaml")]
    pub config: PathBuf,
    /// CLI overrides (highest-precedence layer; spec §5.1).
    #[command(flatten)]
    pub overrides: ConfigOverrideArgs,
}

/// The CLI override layer for the documented `LIMEN_*` knobs.
#[derive(Debug, Args)]
pub struct ConfigOverrideArgs {
    /// Override `server.listen_addr`.
    #[arg(long)]
    pub listen_addr: Option<String>,
    /// Override `metrics.listen_addr`.
    #[arg(long)]
    pub metrics_addr: Option<String>,
    /// Override `flags.provider` (static|file|redis).
    #[arg(long)]
    pub flags_provider: Option<String>,
    /// Override `flags.redis.url`.
    #[arg(long)]
    pub redis_url: Option<String>,
    /// Override `flags.fail_safe_mode` (legacy_only).
    #[arg(long)]
    pub fail_safe_mode: Option<String>,
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

/// Build the merged override layer: environment (lower) overlaid by CLI flags
/// (higher). Both go through the same typed constructor, so a bad value is
/// reported identically regardless of which layer supplied it.
fn merged_overrides(args: &ConfigOverrideArgs) -> anyhow::Result<ConfigOverrides> {
    let cli = ConfigOverrides::from_parts(
        args.listen_addr.clone(),
        args.metrics_addr.clone(),
        args.flags_provider.clone(),
        args.redis_url.clone(),
        args.fail_safe_mode.clone(),
    )?;
    Ok(ConfigOverrides::from_env()?.overlay(cli))
}

/// Load a configuration and run full semantic validation, printing field-level
/// errors. Shared by `run`, `validate-config`, and `print-routes`.
fn load_and_validate(args: &ConfigArgs) -> anyhow::Result<LoadedConfig> {
    let overrides = merged_overrides(&args.overrides)?;
    let loaded = config::load::load(&args.config, &overrides)?;
    if let Err(errors) = config::validate(&loaded.config, &loaded.base_dir) {
        for e in &errors {
            eprintln!("  {e}");
        }
        anyhow::bail!(
            "{} is invalid: {} error(s)",
            args.config.display(),
            errors.len()
        );
    }
    Ok(loaded)
}

async fn cmd_run(args: ConfigArgs) -> anyhow::Result<()> {
    let loaded = load_and_validate(&args)?;
    // Bind the data-plane and control-plane listeners and serve until a
    // shutdown signal.
    crate::http::serve(loaded.config).await
}

fn cmd_validate_config(args: ConfigArgs) -> anyhow::Result<()> {
    let loaded = load_and_validate(&args)?;
    println!(
        "OK: {} is valid ({} route(s))",
        args.config.display(),
        loaded.config.routes.len()
    );
    Ok(())
}

fn cmd_print_routes(args: ConfigArgs) -> anyhow::Result<()> {
    let loaded = load_and_validate(&args)?;
    print_routes(&loaded);
    Ok(())
}

/// Render the resolved routing table as readable per-route blocks.
fn print_routes(loaded: &LoadedConfig) {
    let routes = &loaded.config.routes;
    if routes.is_empty() {
        println!("(no routes configured)");
        return;
    }
    for route in routes {
        let methods = route.r#match.methods.join(",");
        let behavioral = match (&route.contract, route.comparison.has_inline_behavioral()) {
            (Some(reference), _) => format!("contract {reference}"),
            (None, true) => "inline".to_string(),
            (None, false) => "none".to_string(),
        };
        println!("{}", route.id);
        println!("  match:      {methods}  {}", route.r#match.path_prefix);
        println!("  mode:       {}", route.mode.as_str());
        println!(
            "  legacy:     {}",
            route.legacy_upstream.as_deref().unwrap_or("-")
        );
        println!(
            "  new:        {}",
            route.new_upstream.as_deref().unwrap_or("-")
        );
        println!(
            "  comparison: {}, sample_rate={:.2}, max_body_bytes={}",
            if route.comparison.enabled {
                "enabled"
            } else {
                "disabled"
            },
            route.comparison.sample_rate,
            route.comparison.max_body_bytes
        );
        println!("  behavioral: {behavioral}");
    }
}

fn cmd_check_contract(args: CheckContractArgs) -> anyhow::Result<()> {
    let contract = contract_load::load_file(&args.contract)?;

    let semantic = contract_load::validate_semantics(&contract);
    let path_issues = contract_load::validate_paths(&contract);
    let total = semantic.len() + path_issues.len();

    if total == 0 {
        println!(
            "OK: {} (service {:?}, {} route(s)) — schema valid and all JSONPaths within the supported subset",
            args.contract.display(),
            contract.service,
            contract.routes.len()
        );
        return Ok(());
    }

    for issue in &semantic {
        eprintln!("  {issue}");
    }
    for issue in &path_issues {
        let where_ = issue
            .route_id
            .as_deref()
            .map(|r| format!("route {r:?}"))
            .unwrap_or_else(|| "defaults".to_string());
        eprintln!(
            "  {where_} {}: {:?} — {}",
            issue.field, issue.path, issue.error
        );
    }
    anyhow::bail!(
        "{} has {} issue(s): {} schema, {} JSONPath",
        args.contract.display(),
        total,
        semantic.len(),
        path_issues.len()
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

    #[test]
    fn parses_cli_overrides() {
        let cli = Cli::parse_from([
            "limen",
            "run",
            "--listen-addr",
            "0.0.0.0:1",
            "--flags-provider",
            "redis",
        ]);
        match cli.command {
            Command::Run(args) => {
                assert_eq!(args.overrides.listen_addr.as_deref(), Some("0.0.0.0:1"));
                assert_eq!(args.overrides.flags_provider.as_deref(), Some("redis"));
            }
            _ => panic!("expected run"),
        }
    }
}
