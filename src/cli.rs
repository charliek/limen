//! Command-line interface: the `clap` subcommands and their dispatch.
//!
//! Five subcommands mirror the spec (Section 4.5, 10.4, 14):
//! - `run` — bind the data-plane and control-plane listeners and serve.
//! - `validate-config` — semantically validate a configuration file.
//! - `print-routes` — print the resolved routing table for a configuration.
//! - `check-contract` — validate a behavioral contract and its JSONPath usage.
//! - `report` — summarize the mismatches a `diff_sink` directory has collected.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::config::{self, ConfigOverrides, LoadedConfig};
use crate::contract::load as contract_load;
use crate::observability::sink::{self, Report, ReportFilter, REPORT_EXAMPLES_PER_ROUTE};

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
    /// Summarize the mismatches recorded in a `diff_sink` directory.
    Report(ReportArgs),
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

/// Arguments for `report`. No config file is involved: the sink directory is
/// self-describing, so a report can be run anywhere the files are (an operator's
/// laptop, a log-collection box) without the proxy's configuration.
#[derive(Debug, Args)]
pub struct ReportArgs {
    /// Sink directory holding the `mismatches-<date>.jsonl` files.
    #[arg(long)]
    pub dir: PathBuf,
    /// Only report this route id.
    #[arg(long)]
    pub route: Option<String>,
    /// Only include mismatches at or after this RFC 3339 timestamp
    /// (e.g. `2026-07-28T00:00:00Z`).
    #[arg(long)]
    pub since: Option<String>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = ReportFormat::Human)]
    pub format: ReportFormat,
}

/// How `report` renders its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReportFormat {
    /// Aligned, human-readable text.
    Human,
    /// A single JSON document (for scripting and cross-tool joins).
    Json,
}

impl Cli {
    /// Dispatch the parsed command.
    pub async fn run(self) -> anyhow::Result<()> {
        match self.command {
            Command::Run(args) => cmd_run(args).await,
            Command::ValidateConfig(args) => cmd_validate_config(args),
            Command::PrintRoutes(args) => cmd_print_routes(args),
            Command::CheckContract(args) => cmd_check_contract(args),
            Command::Report(args) => cmd_report(args),
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
    crate::http::server::serve(loaded.config, &loaded.base_dir).await
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

fn cmd_report(args: ReportArgs) -> anyhow::Result<()> {
    let since = args
        .since
        .as_deref()
        .map(|s| {
            OffsetDateTime::parse(s, &Rfc3339)
                .map_err(|e| anyhow::anyhow!("--since {s:?} is not an RFC 3339 timestamp: {e}"))
        })
        .transpose()?;
    let filter = ReportFilter {
        route: args.route,
        since,
    };
    let report = sink::read_report(&args.dir, &filter, REPORT_EXAMPLES_PER_ROUTE)
        .map_err(|e| anyhow::anyhow!("cannot read sink directory {}: {e}", args.dir.display()))?;

    match args.format {
        ReportFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        ReportFormat::Human => print_report(&report),
    }
    Ok(())
}

/// The column width that fits every value (in characters, not bytes).
fn width<'a>(values: impl Iterator<Item = &'a str>) -> usize {
    values.map(|v| v.chars().count()).max().unwrap_or(0)
}

/// Render a report as aligned text: a per-route summary table, then the most
/// recent examples per route.
fn print_report(report: &Report) {
    if report.total == 0 {
        println!(
            "no mismatches recorded ({} file(s) read)",
            report.files_read
        );
    } else {
        println!(
            "{} mismatch(es) across {} route(s) ({} file(s) read)",
            report.total,
            report.routes.len(),
            report.files_read
        );
    }
    if report.malformed_lines > 0 {
        println!(
            "warning: {} unparseable line(s) skipped",
            report.malformed_lines
        );
    }
    if report.routes.is_empty() {
        return;
    }

    let id_width = width(report.routes.iter().map(|r| r.route_id.as_str())).max("ROUTE".len());
    println!();
    println!("{:<id_width$}  {:>5}  KINDS", "ROUTE", "COUNT");
    for route in &report.routes {
        let kinds: Vec<String> = route
            .kinds
            .iter()
            .map(|(kind, n)| format!("{kind} {n}"))
            .collect();
        println!(
            "{:<id_width$}  {:>5}  {}",
            route.route_id,
            route.count,
            kinds.join(", ")
        );
    }

    for route in &report.routes {
        if route.examples.is_empty() {
            continue;
        }
        println!();
        println!("{} — {} most recent:", route.route_id, route.examples.len());
        let method_width = width(route.examples.iter().map(|e| e.method.as_str()));
        let path_width = width(route.examples.iter().map(|e| e.path.as_str()));
        for example in &route.examples {
            println!(
                "  {}  {:<method_width$}  {:<path_width$}  {}  {}",
                example.timestamp,
                example.method,
                example.path,
                example.request_id,
                example.mismatch_kinds.join(",")
            );
        }
    }
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
        assert!(matches!(
            Cli::parse_from(["limen", "report", "--dir", "./diffs"]).command,
            Command::Report(_)
        ));
    }

    #[test]
    fn report_args_default_to_human_and_no_filters() {
        match Cli::parse_from(["limen", "report", "--dir", "./diffs"]).command {
            Command::Report(args) => {
                assert_eq!(args.dir, PathBuf::from("./diffs"));
                assert_eq!(args.format, ReportFormat::Human);
                assert!(args.route.is_none() && args.since.is_none());
            }
            _ => panic!("expected report"),
        }
    }

    #[test]
    fn report_accepts_route_since_and_format_filters() {
        match Cli::parse_from([
            "limen",
            "report",
            "--dir",
            "./diffs",
            "--route",
            "get-device",
            "--since",
            "2026-07-28T00:00:00Z",
            "--format",
            "json",
        ])
        .command
        {
            Command::Report(args) => {
                assert_eq!(args.route.as_deref(), Some("get-device"));
                assert_eq!(args.since.as_deref(), Some("2026-07-28T00:00:00Z"));
                assert_eq!(args.format, ReportFormat::Json);
            }
            _ => panic!("expected report"),
        }
    }

    #[test]
    fn report_rejects_a_non_rfc3339_since() {
        let err = cmd_report(ReportArgs {
            dir: PathBuf::from("."),
            route: None,
            since: Some("yesterday".to_string()),
            format: ReportFormat::Human,
        })
        .unwrap_err();
        assert!(err.to_string().contains("RFC 3339"), "{err}");
    }

    #[test]
    fn report_on_a_missing_directory_names_the_directory() {
        let err = cmd_report(ReportArgs {
            dir: PathBuf::from("./definitely-not-a-sink-dir"),
            route: None,
            since: None,
            format: ReportFormat::Human,
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("definitely-not-a-sink-dir"),
            "{err}"
        );
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
