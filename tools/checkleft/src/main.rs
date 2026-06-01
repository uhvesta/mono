use std::io::IsTerminal;
use std::io::stderr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use checkleft::change_detection::environment::CiEnvironment;
use checkleft::change_detection::{ChangePlan, ChangeOverrides, base_revision_from_plan, resolve_change_plan};
use checkleft::check::CheckRegistry;
use checkleft::checks::register_builtin_checks;
use checkleft::config::{ConfigResolver, ConfigResolverOptions};
use checkleft::external::{
    CompositeExternalCheckPackageProvider, ConfiguredExternalCheckPackageProvider,
    DefaultExternalCheckExecutor, ExternalCheckExecutor, ExternalCheckPackageProvider,
    FileExternalCheckPackageProvider, GeneratedExternalCheckPackageProvider,
    NoopExternalCheckExecutor, NoopExternalCheckPackageProvider,
};
use checkleft::input::ChangeSet;
use checkleft::output::{CheckResult, Finding, Location, Severity, SuggestedFix};
use checkleft::runner::Runner;
use checkleft::source_tree::LocalSourceTree;
use checkleft::vcs::{BaseRevision, Vcs, github_pull_request_description};
use clap::{Args, Parser, Subcommand, ValueEnum};
use tracing::info;
use tracing_subscriber::filter::LevelFilter;

#[derive(Debug, Parser)]
#[command(name = "checkleft")]
#[command(about = "Run repository convention checks")]
struct Cli {
    #[arg(long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Run {
        #[command(flatten)]
        config: ConfigArgs,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        base_ref: Option<String>,
        #[arg(long)]
        default_branch: Option<String>,
        #[arg(long, default_value = "human")]
        format: OutputFormat,
    },
    List {
        #[command(flatten)]
        config: ConfigArgs,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        base_ref: Option<String>,
        #[arg(long)]
        default_branch: Option<String>,
    },
}

#[derive(Debug, Args, Clone, Default)]
struct ConfigArgs {
    #[arg(long)]
    external_checks_file: Option<String>,
    #[arg(long)]
    external_checks_url: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Human,
    Json,
}

const CHECKS_PR_DESCRIPTION_ENV: &str = "CHECKS_PR_DESCRIPTION";
const CHECKS_CHANGE_ID_ENV: &str = "CHECKS_CHANGE_ID";
const CHECKS_PR_NUMBER_ENV: &str = "CHECKS_PR_NUMBER";
const CHECKS_REPOSITORY_ENV: &str = "CHECKS_REPOSITORY";
const CHECKS_GITHUB_TOKEN_ENV: &str = "CHECKS_GITHUB_TOKEN";
const CHECKLEFT_EXTERNAL_CHECK_INDEX_ENV: &str = "CHECKLEFT_EXTERNAL_CHECK_INDEX";
const CHECKLEFT_EXTERNAL_PROVIDER_MODE_ENV: &str = "CHECKLEFT_EXTERNAL_PROVIDER_MODE";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExternalProviderMode {
    Auto,
    FileOnly,
    GeneratedOnly,
    Off,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run_cli().await {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run_cli() -> Result<ExitCode> {
    let cli = Cli::parse();
    init_tracing(cli.verbose)?;
    let root = std::env::current_dir()?;
    info!(root = %root.display(), "starting checkleft");

    let vcs = Vcs::detect(&root)?;
    info!(kind = ?vcs.kind(), "detected repository");
    let env = CiEnvironment::from_env();

    match cli.command {
        Commands::Run {
            config,
            all,
            base_ref,
            default_branch,
            format,
        } => {
            let overrides = ChangeOverrides { all, base_ref, default_branch };
            info!("resolving change plan");
            let plan = resolve_change_plan(&env, &vcs, &overrides)?;
            info!("building runner for run");
            let runner = build_runner(
                &root,
                &vcs,
                base_revision_from_plan(&vcs, &plan),
                config.external_checks_file,
                config.external_checks_url,
            )
            .await?;
            info!("resolving changeset for run");
            let changeset = attach_description_context(
                changeset_from_plan(&vcs, &plan)?,
                &vcs,
            )
            .await;
            info!(
                changed_files = changeset.changed_files.len(),
                "resolved changeset for run"
            );
            let run_started_at = Instant::now();
            let mut results = runner.run_changeset(&changeset).await?;
            let elapsed = run_started_at.elapsed();
            sort_results_for_output(&mut results);

            match format {
                OutputFormat::Human => print_human_results(&results, elapsed),
                OutputFormat::Json => print_json_results(&results)?,
            }

            let has_error = results.iter().any(|result| {
                result
                    .findings
                    .iter()
                    .any(|finding| finding.severity == Severity::Error)
            });
            Ok(if has_error {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
        Commands::List {
            config,
            all,
            base_ref,
            default_branch,
        } => {
            let overrides = ChangeOverrides { all, base_ref, default_branch };
            info!("resolving change plan");
            let plan = resolve_change_plan(&env, &vcs, &overrides)?;
            info!("building runner for list");
            let runner = build_runner(
                &root,
                &vcs,
                base_revision_from_plan(&vcs, &plan),
                config.external_checks_file,
                config.external_checks_url,
            )
            .await?;
            info!("resolving changeset for list");
            let changeset = changeset_from_plan(&vcs, &plan)?;
            info!(
                changed_files = changeset.changed_files.len(),
                "resolved changeset for list"
            );
            let checks = runner.list_configured_checks(&changeset)?;
            if checks.is_empty() {
                println!("No configured checks found.");
            } else {
                for check in checks {
                    println!("{check}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn build_runner(
    root: &Path,
    _vcs: &Vcs,
    base_revision: Option<BaseRevision>,
    external_checks_file: Option<String>,
    external_checks_url: Option<String>,
) -> Result<Runner> {
    info!("registering built-in checks");
    let mut registry = CheckRegistry::new();
    register_builtin_checks(&mut registry)?;
    info!("initializing config resolver");
    let resolver = Arc::new(
        ConfigResolver::new_with_options(
            root,
            ConfigResolverOptions {
                external_checks_file,
                external_checks_url,
            },
        )
        .await?,
    );
    info!("initializing source tree");
    let source_tree = Arc::new(LocalSourceTree::with_base_revision(root, base_revision)?);
    info!("initializing external package provider");
    let external_provider = build_external_package_provider(root)?;
    info!("initializing external executor");
    let external_executor = build_external_check_executor(root)?;

    Ok(Runner::with_external(
        Arc::new(registry),
        resolver,
        source_tree,
        external_provider,
        external_executor,
    ))
}

fn changeset_from_plan(vcs: &Vcs, plan: &ChangePlan) -> Result<ChangeSet> {
    match plan {
        ChangePlan::All => vcs.all_files_changeset(),
        ChangePlan::Scoped { base_sha, .. } => vcs.changeset_since(base_sha),
        ChangePlan::Empty { .. } => Ok(ChangeSet::default()),
    }
}

fn build_external_package_provider(root: &Path) -> Result<Arc<dyn ExternalCheckPackageProvider>> {
    let mode = external_provider_mode()?;
    info!(?mode, "resolved external package provider mode");
    if mode == ExternalProviderMode::Off {
        return Ok(Arc::new(NoopExternalCheckPackageProvider));
    }

    let mut providers = Vec::new();
    if mode != ExternalProviderMode::GeneratedOnly {
        providers.push(ConfiguredExternalCheckPackageProvider::new(
            "file",
            Arc::new(FileExternalCheckPackageProvider::new(root)?),
        ));
    }

    let index_path =
        normalize_optional_description(std::env::var(CHECKLEFT_EXTERNAL_CHECK_INDEX_ENV).ok());
    if mode == ExternalProviderMode::GeneratedOnly && index_path.is_none() {
        anyhow::bail!(
            "`{CHECKLEFT_EXTERNAL_PROVIDER_MODE_ENV}=generated-only` requires `{CHECKLEFT_EXTERNAL_CHECK_INDEX_ENV}` to be set"
        );
    }
    if mode != ExternalProviderMode::FileOnly {
        if let Some(index_path) = index_path {
            info!(index_path = %index_path, "loading generated external package index");
            let generated_provider = GeneratedExternalCheckPackageProvider::from_index_path(
                root,
                Path::new(&index_path),
            )?;
            providers.push(ConfiguredExternalCheckPackageProvider::new(
                "generated-index",
                Arc::new(generated_provider),
            ));
        }
    }

    if providers.is_empty() {
        return Ok(Arc::new(NoopExternalCheckPackageProvider));
    }
    Ok(Arc::new(CompositeExternalCheckPackageProvider::new(
        providers,
    )))
}

fn build_external_check_executor(root: &Path) -> Result<Arc<dyn ExternalCheckExecutor>> {
    if external_provider_mode()? == ExternalProviderMode::Off {
        return Ok(Arc::new(NoopExternalCheckExecutor));
    }
    Ok(Arc::new(DefaultExternalCheckExecutor::new(root)?))
}

fn external_provider_mode() -> Result<ExternalProviderMode> {
    parse_external_provider_mode(normalize_optional_description(
        std::env::var(CHECKLEFT_EXTERNAL_PROVIDER_MODE_ENV).ok(),
    ))
}

fn parse_external_provider_mode(raw: Option<String>) -> Result<ExternalProviderMode> {
    match raw.as_deref() {
        None | Some("auto") => Ok(ExternalProviderMode::Auto),
        Some("file-only") => Ok(ExternalProviderMode::FileOnly),
        Some("generated-only") => Ok(ExternalProviderMode::GeneratedOnly),
        Some("off") => Ok(ExternalProviderMode::Off),
        Some(other) => anyhow::bail!(
            "invalid `{CHECKLEFT_EXTERNAL_PROVIDER_MODE_ENV}` value `{other}` (expected one of: auto, file-only, generated-only, off)"
        ),
    }
}

async fn attach_description_context(changeset: ChangeSet, vcs: &Vcs) -> ChangeSet {
    info!("attaching commit and PR metadata");
    let commit_description = normalize_optional_description(vcs.current_commit_description().ok());
    let change_id = resolve_change_id();
    let repository = resolve_repository(vcs);
    let pr_description = normalize_optional_description(
        resolve_pr_description(repository.as_deref(), change_id.as_deref()).await,
    );
    changeset
        .with_commit_description(commit_description)
        .with_change_id(change_id)
        .with_repository(repository)
        .with_pr_description(pr_description)
}

fn resolve_change_id() -> Option<String> {
    [
        std::env::var(CHECKS_CHANGE_ID_ENV),
        std::env::var(CHECKS_PR_NUMBER_ENV),
    ]
    .into_iter()
    .find_map(|value| normalize_optional_description(value.ok()))
}

fn resolve_repository(vcs: &Vcs) -> Option<String> {
    normalize_optional_description(std::env::var(CHECKS_REPOSITORY_ENV).ok())
        .or_else(|| normalize_optional_description(vcs.remote_repo_slug()))
}

async fn resolve_pr_description(
    repository: Option<&str>,
    change_id: Option<&str>,
) -> Option<String> {
    if let Ok(raw) = std::env::var(CHECKS_PR_DESCRIPTION_ENV) {
        if !raw.trim().is_empty() {
            return Some(raw);
        }
    }

    let Some(repository) = repository else {
        return None;
    };
    let Some(change_id) = change_id else {
        return None;
    };

    info!(
        repository = repository,
        change_id = change_id,
        "fetching PR description"
    );
    let github_token = detect_github_token();
    github_pull_request_description(repository, change_id, github_token.as_deref()).await
}

fn init_tracing(verbose: bool) -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(if verbose {
            LevelFilter::INFO
        } else {
            LevelFilter::OFF
        })
        .with_writer(stderr)
        .try_init()
        .map_err(|err| anyhow!("failed to initialize tracing subscriber: {err}"))?;

    Ok(())
}

fn detect_github_token() -> Option<String> {
    [
        std::env::var(CHECKS_GITHUB_TOKEN_ENV),
        std::env::var("GH_TOKEN"),
        std::env::var("GITHUB_TOKEN"),
    ]
    .into_iter()
    .find_map(|value| normalize_optional_description(value.ok()))
}

fn normalize_optional_description(value: Option<String>) -> Option<String> {
    value
        .map(|description| description.trim().to_owned())
        .filter(|description| !description.is_empty())
}

fn print_human_results(results: &[CheckResult], elapsed: Duration) {
    print!(
        "{}",
        render_human_results(results, OutputStyle::detect_for_stdout(), elapsed)
    );
}

fn print_json_results(results: &[CheckResult]) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(results)?);
    Ok(())
}

fn sort_results_for_output(results: &mut [CheckResult]) {
    for result in results.iter_mut() {
        result
            .findings
            .sort_by_key(|finding| severity_sort_key(finding.severity));
    }

    results.sort_by(|left, right| {
        most_severe_finding_sort_key(left)
            .cmp(&most_severe_finding_sort_key(right))
            .then_with(|| left.check_id.cmp(&right.check_id))
    });
}

fn most_severe_finding_sort_key(result: &CheckResult) -> u8 {
    result
        .findings
        .iter()
        .map(|finding| severity_sort_key(finding.severity))
        .min()
        .unwrap_or(u8::MAX)
}

fn severity_sort_key(severity: Severity) -> u8 {
    match severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
    }
}

fn render_human_results(results: &[CheckResult], style: OutputStyle, elapsed: Duration) -> String {
    if results.is_empty() {
        return "No checks ran.\n".to_owned();
    }

    let total_findings: usize = results.iter().map(|result| result.findings.len()).sum();
    if total_findings == 0 {
        return format!(
            "{}: no findings ({} checks run in {})\n",
            style.paint_info("checks"),
            results.len(),
            format!("{}s", elapsed.as_secs())
        );
    }

    let mut output = String::new();
    let mut errors = 0usize;
    let mut warnings = 0usize;
    let mut infos = 0usize;

    for result in results {
        for finding in &result.findings {
            match finding.severity {
                Severity::Error => errors += 1,
                Severity::Warning => warnings += 1,
                Severity::Info => infos += 1,
            }

            output.push_str(&render_finding(result, finding, style));
        }
    }

    output.push_str(&format!(
        "{}: {errors} error(s), {warnings} warning(s), {infos} info finding(s)\n",
        style.paint_bold("summary")
    ));
    output
}

fn render_finding(result: &CheckResult, finding: &Finding, style: OutputStyle) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{}[{}]: {}\n",
        style.paint_severity(finding.severity),
        style.paint_check_id(&result.check_id),
        style.paint_message(&finding.message)
    ));

    let location = finding
        .location
        .as_ref()
        .map(format_location)
        .unwrap_or_else(|| "<unknown>".to_owned());
    out.push_str(&format!("  --> {location}\n"));

    if let Some(remediation) = &finding.remediation {
        let lines: Vec<&str> = remediation.lines().collect();
        if lines.len() > 1 {
            out.push_str(&format!("   = {}:\n", style.paint_help_label("to resolve")));
            let bullet = style.resolution_bullet();
            for line in lines {
                out.push_str(&format!("   {bullet} {}\n", style.paint_help_body(line)));
            }
        } else {
            out.push_str(&format!(
                "   = {}: {}\n",
                style.paint_help_label("to resolve"),
                style.paint_help_body(remediation)
            ));
        }
    }

    if let Some(suggested_fix) = &finding.suggested_fix {
        out.push_str(&format!(
            "   = {}: {}\n",
            style.paint_help_label("fix"),
            style.paint_help_body(&format_fix_summary(suggested_fix))
        ));
    }

    out.push('\n');
    out
}

fn format_location(location: &Location) -> String {
    let path = location.path.display();
    match (location.line, location.column) {
        (Some(line), Some(column)) => format!("{path}:{line}:{column}"),
        (Some(line), None) => format!("{path}:{line}"),
        (None, _) => format!("{path}"),
    }
}

fn format_fix_summary(suggested_fix: &SuggestedFix) -> String {
    format!(
        "{} ({} edit{})",
        suggested_fix.description,
        suggested_fix.edits.len(),
        if suggested_fix.edits.len() == 1 {
            ""
        } else {
            "s"
        }
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ColorLevel {
    None,
    Basic,
    Color256,
    TrueColor,
}

#[derive(Clone, Copy)]
struct OutputStyle {
    level: ColorLevel,
}

impl OutputStyle {
    fn detect_for_stdout() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some();
        if !std::io::stdout().is_terminal() || no_color {
            return Self { level: ColorLevel::None };
        }

        if let Ok(colorterm) = std::env::var("COLORTERM") {
            let ct = colorterm.to_ascii_lowercase();
            if ct == "truecolor" || ct == "24bit" {
                return Self { level: ColorLevel::TrueColor };
            }
        }

        if let Ok(term) = std::env::var("TERM") {
            if term.contains("256color") {
                return Self { level: ColorLevel::Color256 };
            }
        }

        Self { level: ColorLevel::Basic }
    }

    fn paint_bold(self, text: &str) -> String {
        self.paint_ansi(text, "1")
    }

    fn paint_error(self, text: &str) -> String {
        self.paint_ansi(text, "1;31")
    }

    fn paint_warning(self, text: &str) -> String {
        self.paint_ansi(text, "1;33")
    }

    fn paint_info(self, text: &str) -> String {
        self.paint_ansi(text, "1;36")
    }

    fn paint_help_label(self, text: &str) -> String {
        self.paint_ansi(text, "1;32")
    }

    fn paint_message(self, text: &str) -> String {
        self.paint_ansi(text, "1")
    }

    fn paint_check_id(self, text: &str) -> String {
        self.paint_help_body(text)
    }

    fn resolution_bullet(self) -> &'static str {
        if self.level != ColorLevel::None { "○" } else { "-" }
    }

    fn paint_help_body(self, text: &str) -> String {
        match self.level {
            ColorLevel::None => text.to_owned(),
            ColorLevel::Basic => format!("\u{1b}[2m{text}\u{1b}[0m"),
            ColorLevel::Color256 => format!("\u{1b}[38;5;244m{text}\u{1b}[0m"),
            ColorLevel::TrueColor => format!("\u{1b}[38;2;150;150;150m{text}\u{1b}[0m"),
        }
    }

    fn paint_severity(self, severity: Severity) -> String {
        match severity {
            Severity::Error => self.paint_error("error"),
            Severity::Warning => self.paint_warning("warning"),
            Severity::Info => self.paint_info("info"),
        }
    }

    fn paint_ansi(self, text: &str, code: &str) -> String {
        if self.level != ColorLevel::None {
            format!("\u{1b}[{code}m{text}\u{1b}[0m")
        } else {
            text.to_owned()
        }
    }
}

#[cfg(test)]
mod tests;
