//! Command-line interface: the `clap` subcommands and their dispatch.
//!
//! Seven subcommands mirror the spec (Section 4.5, 10.4, 12.1, 14):
//! - `run` — bind the data-plane and control-plane listeners and serve.
//! - `validate-config` — semantically validate a configuration file.
//! - `print-routes` — print the resolved routing table for a configuration.
//! - `check-contract` — validate a behavioral contract and its JSONPath usage.
//! - `report` — summarize the mismatches a `diff_sink` directory has collected.
//! - `verdict` — render a typed campaign verdict from the config, the live
//!   control plane, and the sink (drain, floors, integrity, canary).
//! - `suggest-routes` — classify an observe-mode profile into a draft config.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::config::{self, ConfigOverrides, LoadedConfig};
use crate::contract::load as contract_load;
use crate::draft::{self, DraftOptions, ProfileSource, SuggestOptions};
use crate::observability::sink::{self, Report, ReportFilter, REPORT_EXAMPLES_PER_ROUTE};
use crate::report_html;
use crate::suggest::{DEFAULT_MAX_COMPARE_PATHS, DEFAULT_MIN_SAMPLES};
use crate::verdict;

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
    /// Render a typed campaign verdict: drain the pipeline, assert per-route
    /// comparison floors, reconcile the sink against the engine's counters,
    /// and exit with a documented code (0/10/20/30/40/50).
    Verdict(VerdictArgs),
    /// Classify an observe-mode profile into a draft configuration: what each
    /// route's traffic suggests about comparing it, with the evidence.
    SuggestRoutes(SuggestRoutesArgs),
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

/// Arguments for `report`. The sink directory is self-describing, so
/// `--dir X` alone still reports anywhere the files are (an operator's laptop,
/// a log-collection box) without the proxy's configuration — including with
/// `--format html`, which simply marks everything it was not given as absent.
///
/// The remaining inputs exist only for `--format html`, which joins the
/// artifacts of a whole campaign — the config, a captured verdict, an observe
/// profile, a `/metrics` scrape — into one page. Passing them to the text
/// formats is an error rather than a silent no-op: an operator who believes a
/// verdict was taken into account must not be handed a page that never read it.
#[derive(Debug, Args)]
pub struct ReportArgs {
    /// Sink directory holding the `mismatches-<date>.jsonl` files.
    #[arg(long)]
    pub dir: PathBuf,
    /// Only report this route id. Not available with `--format html`.
    #[arg(long)]
    pub route: Option<String>,
    /// Only include mismatches at or after this RFC 3339 timestamp
    /// (e.g. `2026-07-28T00:00:00Z`). Not available with `--format html`.
    #[arg(long)]
    pub since: Option<String>,
    /// The limen configuration the campaign ran under (`--format html`).
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// A file captured from `limen verdict --format json` (`--format html`).
    #[arg(long)]
    pub verdict: Option<PathBuf>,
    /// A saved `GET /observe/profile` body (`--format html`).
    #[arg(long)]
    pub profile: Option<PathBuf>,
    /// A saved `/metrics` text scrape (`--format html`).
    #[arg(long)]
    pub metrics: Option<PathBuf>,
    /// Write the output here instead of stdout.
    #[arg(long)]
    pub out: Option<PathBuf>,
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
    /// A self-contained HTML status page over the campaign's artifacts.
    Html,
}

/// How `verdict` renders its output. Separate from [`ReportFormat`] so the
/// HTML page cannot be asked for here: a verdict is a typed exit code plus the
/// evidence for it, and a page has no exit code. The page is downstream of a
/// verdict (`report --format html --verdict …`), never a way to take one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum VerdictFormat {
    /// Aligned, human-readable text.
    Human,
    /// A single JSON document (for scripting and cross-tool joins).
    Json,
}

/// Arguments for `verdict`. Unlike `report`, a config file is required: the
/// verdict's floors and route matrix come from it, and the sink directory and
/// control-plane address default from it so one flag set drives everything.
#[derive(Debug, Args)]
pub struct VerdictArgs {
    #[command(flatten)]
    pub config: ConfigArgs,
    /// Sink directory override (default: the config's `diff_sink.dir`,
    /// resolved exactly as `run` resolves it — against the process CWD).
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Control-plane base URL override (default: derived from the config's
    /// `metrics.listen_addr`, with wildcard hosts mapped to `127.0.0.1`).
    #[arg(long)]
    pub control_url: Option<String>,
    /// Trigger the debug sink canary and require it end-to-end (needs
    /// `debug.sink_canary: true` in the running proxy's config).
    #[arg(long, conflicts_with = "offline")]
    pub canary: bool,
    /// Degraded report-only mode: no drain, floors, integrity, or canary.
    /// An offline exit 0 is weaker than an online one — prefer online.
    #[arg(long)]
    pub offline: bool,
    /// Slack added to the longest route shadow timeout to form the drain
    /// deadline (compare + sink flush headroom).
    #[arg(long, default_value_t = 2000)]
    pub drain_slack_ms: u64,
    /// Advanced: replace the computed drain deadline entirely.
    #[arg(long)]
    pub drain_deadline_ms: Option<u64>,
    /// Interval between drain scrapes.
    #[arg(long, default_value_t = 250)]
    pub poll_interval_ms: u64,
    /// Output format.
    #[arg(long, value_enum, default_value_t = VerdictFormat::Human)]
    pub format: VerdictFormat,
}

/// Arguments for `suggest-routes`. Like `verdict`, a config file is required:
/// the route table it classifies and the control-plane address both come from
/// it. The sample rate does **not** — that is read off the profile the proxy
/// wrote, and the config's copy is only cross-checked against it.
#[derive(Debug, Args)]
pub struct SuggestRoutesArgs {
    #[command(flatten)]
    pub config: ConfigArgs,
    /// Control-plane base URL override (default: derived from the config's
    /// `metrics.listen_addr`, with wildcard hosts mapped to `127.0.0.1`).
    #[arg(long, conflicts_with = "profile")]
    pub control_url: Option<String>,
    /// Classify a saved profile document instead of polling a running proxy —
    /// the same JSON `GET /observe/profile` serves. No quiescence poll: a file
    /// is already static.
    #[arg(long)]
    pub profile: Option<PathBuf>,
    /// New upstream for routes that do not configure one. Without it such a
    /// route is drafted `mode: legacy_only`, so the draft is valid whether or
    /// not a `new` service exists yet.
    #[arg(long)]
    pub new_upstream: Option<String>,
    /// Reads below which a route is not classified at all.
    #[arg(long, default_value_t = DEFAULT_MIN_SAMPLES)]
    pub min_samples: u64,
    /// Distinct read paths above which a route is treated as a wildcard proxy.
    #[arg(long, default_value_t = DEFAULT_MAX_COMPARE_PATHS)]
    pub max_compare_paths: u64,
    /// Emit the shadowing form (`comparison.enabled: true`) for suggested
    /// routes.
    ///
    /// PRECONDITION: you have confirmed against the service's source that each
    /// suggested route does not mutate. Observation cannot establish that — it
    /// can prove a route unsafe to compare, never safe — so promotion is a
    /// deliberate human act and this flag is where you take it.
    #[arg(long)]
    pub adopt_suggestions: bool,
    /// Output format: the draft configuration, or the machine surface.
    #[arg(long, value_enum, default_value_t = DraftFormat::Yaml)]
    pub format: DraftFormat,
    /// How long to wait for the profile to stop changing (ignored with
    /// `--profile`).
    #[arg(long, default_value_t = draft::DEFAULT_DRAIN_DEADLINE_MS)]
    pub drain_deadline_ms: u64,
    /// Interval between quiescence polls (ignored with `--profile`).
    #[arg(long, default_value_t = draft::DEFAULT_POLL_INTERVAL_MS)]
    pub poll_interval_ms: u64,
}

/// How `suggest-routes` renders its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DraftFormat {
    /// A complete, loadable draft configuration document.
    Yaml,
    /// One `{route_id, disposition, reason, evidence}` object per route.
    Json,
}

impl Cli {
    /// Dispatch the parsed command. Only `verdict` and `suggest-routes` use
    /// exit codes beyond success; every other command reports failure through
    /// `Err` (exit 1).
    pub async fn run(self) -> anyhow::Result<ExitCode> {
        match self.command {
            Command::Verdict(args) => return cmd_verdict(args).await,
            Command::SuggestRoutes(args) => return cmd_suggest_routes(args).await,
            Command::Run(args) => cmd_run(args).await?,
            Command::ValidateConfig(args) => cmd_validate_config(args)?,
            Command::PrintRoutes(args) => cmd_print_routes(args)?,
            Command::CheckContract(args) => cmd_check_contract(args)?,
            Command::Report(args) => cmd_report(args)?,
        }
        Ok(ExitCode::SUCCESS)
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
        println!("  match:      {methods}  {}", route.r#match.path_display());
        // Only shown when the route conditions on the query — an unconditioned
        // route (the default) needs no line.
        if !route.r#match.query_present.is_empty() {
            println!(
                "  query_present: {}",
                route.r#match.query_present.join(", ")
            );
        }
        if !route.r#match.query_absent.is_empty() {
            println!("  query_absent:  {}", route.r#match.query_absent.join(", "));
        }
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
        // Only shown when the route opted a write into shadowing — the default
        // (reads only) needs no line.
        if !route.comparison.shadow_methods.is_empty() {
            println!(
                "  shadow_methods: {}",
                route.comparison.shadow_methods.join(", ")
            );
        }
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
    if args.format == ReportFormat::Html {
        return cmd_report_html(args);
    }

    // The artifact flags only mean something to the HTML page. Ignoring them
    // here would hand back a report that silently answered a narrower question
    // than the one that was asked.
    let given = flags_given(&[
        ("--config", args.config.is_some()),
        ("--verdict", args.verdict.is_some()),
        ("--profile", args.profile.is_some()),
        ("--metrics", args.metrics.is_some()),
    ]);
    if !given.is_empty() {
        anyhow::bail!(
            "{} only appl{} to --format html; this report would have ignored {}",
            given.join(", "),
            if given.len() == 1 { "ies" } else { "y" },
            if given.len() == 1 { "it" } else { "them" },
        );
    }

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

    let rendered = match args.format {
        ReportFormat::Json => format!("{}\n", serde_json::to_string_pretty(&report)?),
        ReportFormat::Human => render_human_report(&report),
        ReportFormat::Html => unreachable!("handled above"),
    };
    emit(args.out.as_deref(), &rendered)
}

/// Render the HTML status page.
///
/// Exit 0 means *the page was produced*, even when every section of it is a
/// failure: a CI artifact that disappears on a bad run is one nobody looks at,
/// and the page is built so that it cannot render a failure as a success. Only
/// an incoherent invocation or an unwritable destination — cases where no page
/// exists at all — is exit 1.
fn cmd_report_html(args: ReportArgs) -> anyhow::Result<()> {
    // `--route` and `--since` filter *before* aggregation (see
    // `sink::read_report`), so a filtered read of a dirty sink can reconcile
    // to zero and render a page that looks clean. clap cannot express a
    // conflict against one *value* of an enum flag, so it is enforced here.
    let given = flags_given(&[
        ("--route", args.route.is_some()),
        ("--since", args.since.is_some()),
    ]);
    if !given.is_empty() {
        anyhow::bail!(
            "{} cannot be combined with --format html: the filter applies before aggregation, \
             so a filtered page could render a dirty sink as a clean one",
            given.join(" and "),
        );
    }

    let page = report_html::render_report(&report_html::Inputs {
        sink_dir: args.dir,
        config: args.config,
        verdict: args.verdict,
        profile: args.profile,
        metrics: args.metrics,
    });
    emit(args.out.as_deref(), &page)
}

/// The flags among these that were passed. Both of `report`'s guards refuse a
/// combination rather than quietly ignore half of it, and both have to name
/// what they refused.
fn flags_given(flags: &[(&'static str, bool)]) -> Vec<&'static str> {
    flags
        .iter()
        .filter(|(_, given)| *given)
        .map(|(name, _)| *name)
        .collect()
}

/// Write rendered output to `--out`, or to stdout when it was not given.
fn emit(out: Option<&Path>, rendered: &str) -> anyhow::Result<()> {
    match out {
        Some(path) => std::fs::write(path, rendered)
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display())),
        None => {
            print!("{rendered}");
            Ok(())
        }
    }
}

async fn cmd_verdict(args: VerdictArgs) -> anyhow::Result<ExitCode> {
    let loaded = load_and_validate(&args.config)?;
    let config = &loaded.config;

    // Every unavailable input is the typed exit 50, never an untyped error:
    // wrappers branch on the code, and a fail-closed verdict must be
    // distinguishable from a crashed tool.
    let sink_dir = match args
        .dir
        .or_else(|| config.diff_sink.as_ref().map(|s| s.dir.clone()))
    {
        Some(dir) => dir,
        None => {
            return render_unavailable(
                args.format,
                "no sink directory: the config has no `diff_sink` block and no --dir was given",
            );
        }
    };
    let control_base = args
        .control_url
        .map(|u| u.trim_end_matches('/').to_string())
        .unwrap_or_else(|| verdict::control_base_from_listen_addr(&config.metrics.listen_addr));
    let metrics_path = if config.metrics.path.starts_with('/') {
        config.metrics.path.clone()
    } else {
        format!("/{}", config.metrics.path)
    };
    let drain_deadline = args
        .drain_deadline_ms
        .map(Duration::from_millis)
        .unwrap_or_else(|| {
            verdict::drain_deadline(config, Duration::from_millis(args.drain_slack_ms))
        });

    let opts = verdict::VerdictOptions {
        sink_dir,
        control_base,
        metrics_path,
        canary: args.canary,
        offline: args.offline,
        drain_deadline,
        poll_interval: Duration::from_millis(args.poll_interval_ms),
    };

    match verdict::run_verdict(config, &opts).await {
        Ok(report) => {
            match args.format {
                VerdictFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
                VerdictFormat::Human => print!("{}", verdict::render_human(&report)),
            }
            Ok(ExitCode::from(report.exit_code))
        }
        Err(e) => render_unavailable(args.format, &e.0),
    }
}

/// Report an input-unavailable verdict (exit 50) in the requested format.
fn render_unavailable(format: VerdictFormat, detail: &str) -> anyhow::Result<ExitCode> {
    match format {
        VerdictFormat::Json => println!(
            "{}",
            serde_json::json!({
                "mode": "unavailable",
                "verdict": "input-unavailable",
                "exit_code": verdict::EXIT_INPUT_UNAVAILABLE,
                "error": detail,
            })
        ),
        VerdictFormat::Human => {
            println!("LIMEN VERDICT: INPUT UNAVAILABLE");
            println!("  {detail}");
            println!(
                "  This is a tooling failure, not a clean run — it must never be read as \
                 0 mismatches."
            );
            println!(
                "  exit {} — input-unavailable",
                verdict::EXIT_INPUT_UNAVAILABLE
            );
        }
    }
    Ok(ExitCode::from(verdict::EXIT_INPUT_UNAVAILABLE))
}

async fn cmd_suggest_routes(args: SuggestRoutesArgs) -> anyhow::Result<ExitCode> {
    let loaded = load_and_validate(&args.config)?;
    let config = &loaded.config;

    let source = match args.profile {
        Some(path) => ProfileSource::File(path),
        None => {
            let base = args
                .control_url
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| {
                    verdict::control_base_from_listen_addr(&config.metrics.listen_addr)
                });
            let metrics_path = if config.metrics.path.starts_with('/') {
                config.metrics.path.clone()
            } else {
                format!("/{}", config.metrics.path)
            };
            ProfileSource::ControlPlane { base, metrics_path }
        }
    };

    let opts = SuggestOptions {
        source,
        min_samples: args.min_samples,
        max_compare_paths: args.max_compare_paths,
        // The third threshold, the sample rate, is not passed here: it is read
        // off the profile document the proxy wrote, and the config's copy is
        // only cross-checked against it.
        drain_deadline: Duration::from_millis(args.drain_deadline_ms),
        poll_interval: Duration::from_millis(args.poll_interval_ms),
    };
    let draft_opts = DraftOptions {
        new_upstream: args.new_upstream,
        adopt: args.adopt_suggestions,
        base_dir: loaded.base_dir.clone(),
    };

    let outcome = match draft::run_suggest_routes(config, &opts).await {
        Ok(outcome) => outcome,
        Err(e) => return suggest_failed(args.format, &e),
    };

    // The document goes to stdout so it can be redirected into a file;
    // everything advisory goes to stderr so that redirection stays clean.
    match args.format {
        DraftFormat::Json => println!("{}", draft::render_json(&outcome.suggestions)?),
        DraftFormat::Yaml => print!(
            "{}",
            draft::render_yaml(config, &outcome.suggestions, &draft_opts)?
        ),
    }
    for warning in &outcome.warnings {
        eprintln!("warning: {warning}");
    }
    // Only when a draft was actually emitted: `--adopt-suggestions` changes
    // nothing about the JSON surface, and a note claiming comparison was
    // enabled would be describing a document that does not exist.
    if draft_opts.adopt && args.format == DraftFormat::Yaml {
        eprintln!(
            "note: --adopt-suggestions emitted comparison.enabled: true — every enabled route \
             dispatches a shadow request. Confirm each against the service's source before \
             running this draft."
        );
    }
    Ok(ExitCode::from(outcome.exit_code))
}

/// Report a `suggest-routes` failure in the requested format, exiting with its
/// typed code. Never a bare error: wrappers branch on 40 vs 50, and a draft
/// that was never produced must not be mistaken for one that suggested nothing.
fn suggest_failed(format: DraftFormat, error: &draft::SuggestError) -> anyhow::Result<ExitCode> {
    let code = error.exit_code();
    match format {
        DraftFormat::Json => println!(
            "{}",
            serde_json::json!({
                "outcome": error.name(),
                "exit_code": code,
                "error": error.to_string(),
            })
        ),
        DraftFormat::Yaml => {
            eprintln!("LIMEN SUGGEST-ROUTES FAILED: {}", error.name());
            eprintln!("  {error}");
            eprintln!("  No draft was emitted — this is a tooling failure, not a suggestion.");
            eprintln!("  exit {code} — {}", error.name());
        }
    }
    Ok(ExitCode::from(code))
}

/// The column width that fits every value (in characters, not bytes).
fn width<'a>(values: impl Iterator<Item = &'a str>) -> usize {
    values.map(|v| v.chars().count()).max().unwrap_or(0)
}

/// Render a report as aligned text: a per-route summary table, then the most
/// recent examples per route.
fn render_human_report(report: &Report) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    if report.total == 0 {
        let _ = writeln!(
            out,
            "no mismatches recorded ({} file(s) read)",
            report.files_read
        );
    } else {
        let _ = writeln!(
            out,
            "{} mismatch(es) across {} route(s) ({} file(s) read)",
            report.total,
            report.routes.len(),
            report.files_read
        );
    }
    if report.malformed_lines > 0 {
        let _ = writeln!(
            out,
            "warning: {} unparseable line(s) skipped",
            report.malformed_lines
        );
    }
    if report.routes.is_empty() {
        return out;
    }

    let id_width = width(report.routes.iter().map(|r| r.route_id.as_str())).max("ROUTE".len());
    let _ = writeln!(out);
    let _ = writeln!(out, "{:<id_width$}  {:>5}  KINDS", "ROUTE", "COUNT");
    for route in &report.routes {
        let kinds: Vec<String> = route
            .kinds
            .iter()
            .map(|(kind, n)| format!("{kind} {n}"))
            .collect();
        let _ = writeln!(
            out,
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
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "{} — {} most recent:",
            route.route_id,
            route.examples.len()
        );
        let method_width = width(route.examples.iter().map(|e| e.method.as_str()));
        let path_width = width(route.examples.iter().map(|e| e.path.as_str()));
        for example in &route.examples {
            let _ = writeln!(
                out,
                "  {}  {:<method_width$}  {:<path_width$}  {}  {}",
                example.timestamp,
                example.method,
                example.path,
                example.request_id,
                example.mismatch_kinds.join(",")
            );
        }
    }
    out
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

    /// `report --dir X` and nothing else — the invocation every other one is a
    /// variation on.
    fn report_args(dir: &str, format: ReportFormat) -> ReportArgs {
        ReportArgs {
            dir: PathBuf::from(dir),
            route: None,
            since: None,
            config: None,
            verdict: None,
            profile: None,
            metrics: None,
            out: None,
            format,
        }
    }

    #[test]
    fn report_rejects_a_non_rfc3339_since() {
        let mut args = report_args(".", ReportFormat::Human);
        args.since = Some("yesterday".to_string());
        let err = cmd_report(args).unwrap_err();
        assert!(err.to_string().contains("RFC 3339"), "{err}");
    }

    #[test]
    fn report_on_a_missing_directory_names_the_directory() {
        let err = cmd_report(report_args(
            "./definitely-not-a-sink-dir",
            ReportFormat::Human,
        ))
        .unwrap_err();
        assert!(
            err.to_string().contains("definitely-not-a-sink-dir"),
            "{err}"
        );
    }

    #[test]
    fn the_html_page_refuses_the_pre_aggregation_filters() {
        // `--route`/`--since` filter records *before* they are aggregated, so a
        // filtered page could reconcile a dirty sink to zero and render green.
        for (flag, mut args) in [
            ("--route", report_args(".", ReportFormat::Html)),
            ("--since", report_args(".", ReportFormat::Html)),
        ] {
            if flag == "--route" {
                args.route = Some("a".to_string());
            } else {
                args.since = Some("2026-07-28T00:00:00Z".to_string());
            }
            let err = cmd_report(args).unwrap_err();
            assert!(err.to_string().contains(flag), "{err}");
            assert!(err.to_string().contains("html"), "{err}");
        }
    }

    #[test]
    fn the_text_formats_refuse_the_artifact_flags_rather_than_ignore_them() {
        let mut args = report_args(".", ReportFormat::Json);
        args.verdict = Some(PathBuf::from("verdict.json"));
        let err = cmd_report(args).unwrap_err();
        assert!(err.to_string().contains("--verdict"), "{err}");
        assert!(err.to_string().contains("html"), "{err}");
    }

    /// An unreadable sink directory is a *section* of the HTML page, never a
    /// process failure: the page has to exist for CI to publish it.
    #[test]
    fn the_html_page_survives_every_missing_input() {
        let out = tempfile::tempdir().unwrap().path().join("gone");
        let mut args = report_args("./definitely-not-a-sink-dir", ReportFormat::Html);
        args.out = Some(out.clone());
        // The tempdir is already gone, so the write fails: that *is* exit 1.
        assert!(cmd_report(args).is_err());

        let dir = tempfile::tempdir().unwrap();
        let page = dir.path().join("report.html");
        let mut args = report_args("./definitely-not-a-sink-dir", ReportFormat::Html);
        args.out = Some(page.clone());
        cmd_report(args).unwrap();
        let html = std::fs::read_to_string(&page).unwrap();
        assert!(html.contains("INCOMPLETE"), "{html}");
    }

    #[test]
    fn report_accepts_the_html_artifact_flags() {
        match Cli::parse_from([
            "limen",
            "report",
            "--dir",
            "./diffs",
            "--format",
            "html",
            "--config",
            "limen.config.yaml",
            "--verdict",
            "verdict.json",
            "--profile",
            "profile.json",
            "--metrics",
            "metrics.txt",
            "--out",
            "report.html",
        ])
        .command
        {
            Command::Report(args) => {
                assert_eq!(args.format, ReportFormat::Html);
                assert_eq!(args.config, Some(PathBuf::from("limen.config.yaml")));
                assert_eq!(args.verdict, Some(PathBuf::from("verdict.json")));
                assert_eq!(args.profile, Some(PathBuf::from("profile.json")));
                assert_eq!(args.metrics, Some(PathBuf::from("metrics.txt")));
                assert_eq!(args.out, Some(PathBuf::from("report.html")));
            }
            _ => panic!("expected report"),
        }
    }

    /// A verdict is a typed exit code; a page has none. Keeping the two format
    /// vocabularies separate is what stops `verdict --format html` from
    /// becoming a way to take a verdict that cannot fail.
    #[test]
    fn verdict_has_no_html_format() {
        let err = Cli::try_parse_from(["limen", "verdict", "--format", "html"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
        assert!(matches!(
            Cli::parse_from(["limen", "verdict", "--format", "json"]).command,
            Command::Verdict(VerdictArgs {
                format: VerdictFormat::Json,
                ..
            })
        ));
    }

    #[test]
    fn suggest_routes_defaults_match_the_documented_vocabulary() {
        match Cli::parse_from(["limen", "suggest-routes", "-c", "x.yaml"]).command {
            Command::SuggestRoutes(args) => {
                assert_eq!(args.format, DraftFormat::Yaml);
                assert_eq!(args.min_samples, DEFAULT_MIN_SAMPLES);
                assert_eq!(args.max_compare_paths, DEFAULT_MAX_COMPARE_PATHS);
                assert_eq!(args.drain_deadline_ms, 2000);
                assert_eq!(args.poll_interval_ms, 250);
                // The default must be the non-shadowing draft: a flag that
                // defaulted on would make promotion an accident.
                assert!(!args.adopt_suggestions);
                assert!(args.profile.is_none() && args.control_url.is_none());
            }
            _ => panic!("expected suggest-routes"),
        }
    }

    #[test]
    fn suggest_routes_refuses_both_profile_sources_at_once() {
        // A saved file and a live proxy are different claims about what is
        // being classified; silently preferring one would hide the other.
        let err = Cli::try_parse_from([
            "limen",
            "suggest-routes",
            "--profile",
            "p.json",
            "--control-url",
            "http://127.0.0.1:9090",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn the_adopt_flag_states_its_precondition() {
        let mut command = Cli::command();
        let help = command.render_long_help().to_string();
        let suggest = Cli::command()
            .get_subcommands()
            .find(|c| c.get_name() == "suggest-routes")
            .expect("subcommand")
            .clone()
            .render_long_help()
            .to_string();
        assert!(help.contains("suggest-routes"));
        assert!(
            suggest.contains("PRECONDITION") && suggest.contains("does not mutate"),
            "{suggest}"
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
