use std::collections::HashSet;
use std::io::IsTerminal;
use std::io::stderr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use checkleft::annotate::check_run;
use checkleft::annotate::sarif::{to_sarif, write_sarif};
use checkleft::annotate::upload::{SarifUploadContext, upload_sarif};
use checkleft::annotate::{Annotation, annotation_from_finding, cap_gha_annotations, format_gha_workflow_commands};
use checkleft::change_detection::environment::{CiEnvironment, resolve_head_sha, resolve_owner_repo};
use checkleft::change_detection::scenario::Scenario;
use checkleft::change_detection::{ChangeOverrides, ChangePlan, base_revision_from_plan, resolve_change_plan};
use checkleft::check::CheckRegistry;
use checkleft::checks::register_builtin_checks;
use checkleft::config::{ConfigResolver, ConfigResolverOptions};
use checkleft::external::FixInvocationOutcome;
use checkleft::external::{
    BundledExternalCheckPackageProvider, CompositeExternalCheckPackageProvider, ConfiguredExternalCheckPackageProvider,
    DefaultExternalCheckExecutor, ExternalCheckExecutor, ExternalCheckPackageProvider,
    FileExternalCheckPackageProvider, GeneratedExternalCheckPackageProvider, NoopExternalCheckExecutor,
    NoopExternalCheckPackageProvider,
};
use checkleft::input::{ChangeKind, ChangeSet, ChangedFile};
use checkleft::install::{
    InstallOutcome, UninstallOutcome, install_pre_push_hook, pre_push_path, uninstall_pre_push_hook,
};
use checkleft::output::{CheckResult, Finding, Location, Severity, SuggestedFix};
use checkleft::progress::render::TermRenderer;
use checkleft::progress::{DEFAULT_DEBOUNCE, LiveProgress, NoopProgressReporter, ProgressReporter, RenderFindings};
use checkleft::runner::{DEFAULT_FIX_PASSES, Runner};
use checkleft::source_tree::LocalSourceTree;
use checkleft::vcs::{BaseRevision, Vcs, github_pr_number_for_branch, github_pull_request_description};
use clap::{Args, Parser, Subcommand, ValueEnum};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Args, Clone)]
struct RunArgs {
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
    /// Show the interactive progress UI (per-check status lines + findings that
    /// stream into a scrolling log). Auto-detected by default: on for an
    /// interactive, color-capable terminal; off for pipes, CI, `NO_COLOR`, and
    /// non-human `--format`. `--show-progress=false` forces it off and yields
    /// output byte-identical to the non-interactive path; `--show-progress`
    /// (or `=true`) forces it on.
    #[arg(long, num_args = 0..=1, default_missing_value = "true", value_name = "BOOL")]
    show_progress: Option<bool>,
    /// Emit findings to a GitHub-UI annotation backend after the run. Repeatable,
    /// so several backends can be active at once; default is none (off — output
    /// is unchanged). Supported: `check-run` (POST findings to the GitHub Check
    /// Runs API; needs a token with `Checks: write` via
    /// `CHECKS_GITHUB_TOKEN`/`GITHUB_TOKEN`), `sarif` (write SARIF 2.1.0 JSON
    /// to `--annotations-out=<path>`), `gha` (emit `::error::`/`::warning::`/
    /// `::notice::` workflow-command lines to stderr for GitHub Actions). `none`
    /// is an explicit no-op.
    #[arg(long = "annotations", value_name = "MODE")]
    annotations: Vec<AnnotationBackend>,
    /// File path for annotation backends that write to a file (e.g. `--annotations=sarif`).
    #[arg(long, value_name = "PATH")]
    annotations_out: Option<PathBuf>,
    /// Make a failure to *post* annotations fatal (non-zero exit) instead of the
    /// default: logging a warning and preserving checkleft's content-driven exit
    /// code. Off by default, so a posting failure never turns a clean run red nor
    /// masks a dirty one.
    #[arg(long)]
    annotations_strict: bool,
    /// Upload SARIF findings to GitHub code scanning (POST /repos/{owner}/{repo}/code-scanning/sarifs).
    /// Requires a GitHub token with the `security_events` scope (checked in order:
    /// CHECKS_GITHUB_TOKEN, GH_TOKEN, GITHUB_TOKEN, `gh auth token`). Can be combined
    /// with `--annotations=sarif --annotations-out` to also write SARIF to a file.
    /// Non-fatal when the repository, commit SHA, or token cannot be resolved, or when
    /// the API call fails — checkleft logs a warning and continues.
    #[arg(long)]
    upload: bool,
}

/// Arguments for `checkleft fix`.
#[derive(Debug, Args, Clone)]
struct FixArgs {
    #[command(flatten)]
    run_args: RunArgs,
    /// Allow fixing files that have uncommitted modifications in the working
    /// tree. When false, dirty files are excluded from the fixable set.
    #[arg(long, num_args = 0..=1, default_missing_value = "true", default_value = "true", value_name = "BOOL")]
    allow_dirty: bool,
    /// Re-run checks after applying fixes and report any remaining failures.
    #[arg(long, num_args = 0..=1, default_missing_value = "true", default_value = "true", value_name = "BOOL")]
    verify: bool,
    /// Maximum fix passes (re-apply fixes until stable or the cap is hit). Default: 10.
    #[arg(long)]
    max_passes: Option<u32>,
    /// Restrict fixing to files under these paths (further intersects the
    /// failing-file set; absent means all failing files are candidates).
    #[arg(value_name = "PATHS")]
    paths: Vec<PathBuf>,
}

/// Explicit log-level override (higher precedence than -v count and RUST_LOG).
#[derive(Debug, Clone, Copy, ValueEnum)]
enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            LogLevel::Off => "off",
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "checkleft")]
#[command(version = option_env!("CHECKLEFT_BUILD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")))]
#[command(about = "Run repository convention checks")]
struct Cli {
    /// Enable verbose tracing output (INFO level). Repeat for more detail:
    /// -v=INFO, -vv=DEBUG, -vvv=TRACE. Alias: --verbose.
    #[arg(short = 'v', long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Set an explicit log level, overriding -v and RUST_LOG.
    /// Precedence: --log-level > -v > RUST_LOG > default (off).
    #[arg(long, global = true, value_name = "LEVEL")]
    log_level: Option<LogLevel>,

    /// Write tracing output to this file instead of stderr.
    /// The file is created or truncated at startup.
    #[arg(long, global = true, value_name = "PATH")]
    log_file: Option<PathBuf>,

    // Top-level run args: active when no subcommand is given (bare `checkleft` == `checkleft run`).
    #[command(flatten)]
    run_args: RunArgs,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Run(RunArgs),
    /// Apply fixes to files that are failing checks. Reuses `run`'s discovery
    /// machinery to find failing files, then applies each check's declared fix
    /// mechanism. Re-runs each check after fixing to report residual failures.
    Fix(FixArgs),
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
    // TEMPORARY: bake-period parity check (P844 migration step 2).
    // Resolves the change plan and prints base_sha + changed_files without running checks.
    // Remove once checks.sh scoping is retired.
    ShowPlan {
        #[arg(long)]
        base_ref: Option<String>,
        #[arg(long)]
        default_branch: Option<String>,
    },
    /// Install a git `pre-push` hook that runs `checkleft run` against the
    /// outgoing changes before each push.
    Install {
        /// Remove the installed hook instead of installing it
        /// (equivalent to `checkleft uninstall`).
        #[arg(long)]
        remove: bool,
    },
    /// Remove the git `pre-push` hook installed by `checkleft install`.
    Uninstall,
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

/// A GitHub-UI annotation backend selectable via `--annotations`.
///
/// Only shipped backends appear here (clap renders the variants kebab-cased:
/// `check-run`, `sarif`, `gha`, `none`); the enum grows one variant per backend as it lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AnnotationBackend {
    /// Post findings to the GitHub Check Runs API. CI-agnostic (GitHub Actions,
    /// Buildkite, or anywhere a token reaches it); renders as a checkleft-named
    /// check plus inline PR-diff annotations.
    CheckRun,
    /// Write SARIF 2.1.0 JSON to `--annotations-out=<path>`.
    Sarif,
    /// Emit `::error::` / `::warning::` / `::notice::` workflow-command lines
    /// to stderr for GitHub Actions. Self-disables when `GITHUB_ACTIONS` is not
    /// `true`/`1`.
    Gha,
    /// Explicit no-op; selects no backend.
    None,
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
    init_tracing(cli.verbose, cli.log_level, cli.log_file.clone())?;
    let root = std::env::current_dir()?;
    info!(root = %root.display(), "starting checkleft");

    let vcs = Vcs::detect(&root)?;
    info!(kind = ?vcs.kind(), "detected repository");
    let env = CiEnvironment::from_env();

    let Cli {
        verbose: _,
        log_level: _,
        log_file: _,
        run_args: default_run_args,
        command,
    } = cli;

    match command {
        None => dispatch_run(default_run_args, &root, &vcs, &env).await,
        Some(Commands::Run(args)) => dispatch_run(args, &root, &vcs, &env).await,
        Some(Commands::Fix(args)) => dispatch_fix(args, &root, &vcs, &env).await,
        Some(Commands::List {
            config,
            all,
            base_ref,
            default_branch,
        }) => {
            let overrides = ChangeOverrides {
                all,
                base_ref,
                default_branch,
            };
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
        // TEMPORARY: bake-period parity check (P844 migration step 2). Remove once checks.sh is retired.
        Some(Commands::ShowPlan {
            base_ref,
            default_branch,
        }) => {
            let overrides = ChangeOverrides {
                all: false,
                base_ref,
                default_branch,
            };
            let plan = resolve_change_plan(&env, &vcs, &overrides)?;
            match &plan {
                ChangePlan::All => println!("plan=all"),
                ChangePlan::Empty { .. } => println!("plan=empty"),
                ChangePlan::Scoped { base_sha, scenario } => {
                    let changeset = changeset_from_plan(&vcs, &plan)?;
                    let scenario_str = match scenario {
                        Scenario::PullRequest { base_branch } => {
                            format!("pull-request({base_branch})")
                        }
                        Scenario::MergeQueue => "merge-queue".to_owned(),
                        Scenario::PushToDefault => "push-to-default".to_owned(),
                        Scenario::PushToBranch { branch } => {
                            format!("push-to-branch({branch})")
                        }
                        Scenario::Local => "local".to_owned(),
                    };
                    println!("base_sha={base_sha}");
                    println!("changed_files={}", changeset.changed_files.len());
                    println!("scenario={scenario_str}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Install { remove }) => dispatch_install(&root, remove),
        Some(Commands::Uninstall) => dispatch_install(&root, true),
    }
}

/// Install or remove the git `pre-push` hook. `remove == true` is the
/// `checkleft uninstall` / `checkleft install --remove` path.
///
/// Output is plain and confident; the only mention of jj's native-push
/// caveat lives in the userdoc (README), not here.
fn dispatch_install(root: &Path, remove: bool) -> Result<ExitCode> {
    let Some(hooks_dir) = resolve_git_hooks_dir(root) else {
        eprintln!(
            "checkleft: not a git repository (no git hooks directory found under {}). \
             Git hooks can only be installed inside a git repository.",
            root.display(),
        );
        return Ok(ExitCode::from(1));
    };
    if !hooks_dir.exists() {
        eprintln!(
            "checkleft: no git hooks directory at {}. Create it (or re-initialise \
             the repository), then retry.",
            hooks_dir.display(),
        );
        return Ok(ExitCode::from(1));
    }
    let hook_path = pre_push_path(&hooks_dir);

    if remove {
        match uninstall_pre_push_hook(&hooks_dir)? {
            UninstallOutcome::Removed => {
                println!("Removed checkleft pre-push hook from {}.", hook_path.display());
            }
            UninstallOutcome::NotInstalled => {
                println!(
                    "No checkleft pre-push hook found at {}; nothing to remove.",
                    hook_path.display(),
                );
            }
            UninstallOutcome::RefusedForeign => {
                println!(
                    "{} is not managed by checkleft; leaving it untouched.",
                    hook_path.display(),
                );
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    let checkleft_bin = current_checkleft_bin();
    match install_pre_push_hook(&hooks_dir, &checkleft_bin)? {
        InstallOutcome::Installed => {
            println!("Installed checkleft pre-push hook at {}.", hook_path.display());
        }
        InstallOutcome::Refreshed => {
            println!("Updated checkleft pre-push hook at {}.", hook_path.display());
        }
        InstallOutcome::AlreadyCurrent => {
            println!("checkleft pre-push hook already installed at {}.", hook_path.display());
        }
        InstallOutcome::RefusedForeign => {
            eprintln!(
                "checkleft: {} already exists and was not installed by checkleft. \
                 Remove or merge it, then re-run `checkleft install`.",
                hook_path.display(),
            );
            return Ok(ExitCode::from(1));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Absolute path to the currently-running checkleft binary, for embedding
/// in the installed hook. Falls back to a bare `checkleft` (PATH lookup at
/// push time) if the executable path cannot be resolved.
fn current_checkleft_bin() -> String {
    std::env::current_exe()
        .ok()
        .map(|p| p.canonicalize().unwrap_or(p))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "checkleft".to_owned())
}

/// Run `git` in `root` and return trimmed stdout, or `None` if the command
/// fails or produces no output.
fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let trimmed = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Resolve the git hooks directory for the repository at `root`. Returns
/// `None` when `root` is not inside a git repository. Honours an explicit
/// `core.hooksPath` config, falling back to the standard `<git-dir>/hooks`.
fn resolve_git_hooks_dir(root: &Path) -> Option<PathBuf> {
    // Must be inside a git repository.
    git_output(root, &["rev-parse", "--git-dir"])?;
    // Honour an explicit core.hooksPath override.
    if let Some(hooks_path) = git_output(root, &["config", "--get", "core.hooksPath"]) {
        let path = PathBuf::from(&hooks_path);
        return Some(if path.is_absolute() { path } else { root.join(path) });
    }
    let hooks = git_output(root, &["rev-parse", "--git-path", "hooks"])?;
    let path = PathBuf::from(&hooks);
    Some(if path.is_absolute() { path } else { root.join(path) })
}

async fn dispatch_run(
    RunArgs {
        config,
        all,
        base_ref,
        default_branch,
        format,
        show_progress,
        annotations,
        annotations_out,
        annotations_strict,
        upload,
    }: RunArgs,
    root: &Path,
    vcs: &Vcs,
    env: &CiEnvironment,
) -> Result<ExitCode> {
    let overrides = ChangeOverrides {
        all,
        base_ref,
        default_branch,
    };
    info!("resolving change plan");
    let plan = resolve_change_plan(env, vcs, &overrides)?;
    info!("building runner for run");
    let runner = build_runner(
        root,
        vcs,
        base_revision_from_plan(vcs, &plan),
        config.external_checks_file,
        config.external_checks_url,
    )
    .await?;
    info!("resolving changeset for run");
    let changeset = attach_description_context(changeset_from_plan(vcs, &plan)?, vcs, env, &plan).await;
    info!(
        changed_files = changeset.changed_files.len(),
        "resolved changeset for run"
    );

    // The progress UI is purely additive on the interactive path. When it is
    // disabled the reporter is a no-op and output is byte-identical to before.
    let style = OutputStyle::detect_for_stdout();
    let progress_enabled = matches!(format, OutputFormat::Human)
        && should_show_progress(
            show_progress,
            style.level,
            std::io::stdout().is_terminal(),
            stderr().is_terminal(),
            detect_ci(),
        );
    let mut live = progress_enabled.then(|| LiveProgress::new(Box::new(TermRenderer::stdout()), DEFAULT_DEBOUNCE));
    let reporter: Arc<dyn ProgressReporter> = match &live {
        Some(progress) => progress.reporter(make_render_findings(style)),
        None => Arc::new(NoopProgressReporter),
    };

    let run_started_at = Instant::now();
    let mut results = runner.run_changeset_with_progress(&changeset, reporter).await?;
    let elapsed = run_started_at.elapsed();
    sort_results_for_output(&mut results);
    let total_findings: usize = results.iter().map(|r| r.findings.len()).sum();
    info!(
        elapsed_ms = elapsed.as_millis(),
        checks_ran = results.len(),
        total_findings,
        "run complete"
    );

    match format {
        OutputFormat::Human => {
            if let Some(mut progress) = live.take() {
                // Stop the render loop, leaving the final per-check status block on
                // screen; the findings already streamed into the log area above it,
                // so only the summary footer remains to print.
                progress.finalize();
                print!("{}", render_human_footer(&results, style, elapsed));
            } else {
                print_human_results(&results, elapsed);
            }
        }
        OutputFormat::Json => print_json_results(&results)?,
    }

    // Generate SARIF once if needed for file writing or upload.
    let needs_sarif = upload || annotations.contains(&AnnotationBackend::Sarif);
    let sarif_doc = if needs_sarif { Some(to_sarif(&results)) } else { None };

    if annotations.contains(&AnnotationBackend::Sarif) {
        let path = annotations_out
            .as_deref()
            .ok_or_else(|| anyhow!("--annotations=sarif requires --annotations-out=<path>"))?;
        write_sarif(&results, path)?;
    }
    if annotations.contains(&AnnotationBackend::Gha) {
        emit_gha_annotations(&results, env);
    }

    if upload {
        let sarif = sarif_doc.as_ref().expect("sarif_doc set when upload=true");
        attempt_sarif_upload(sarif, env, vcs).await;
    }

    let has_error = results
        .iter()
        .any(|result| result.findings.iter().any(|f| f.severity == Severity::Error));

    // Annotation backends are a side output: they post the same `results` to a
    // GitHub-UI surface and never change the content-driven exit code below.
    // In strict mode a posting failure is fatal (propagated as an `Err`).
    emit_annotations(&annotations, annotations_strict, &results, env, vcs).await?;

    Ok(if has_error {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// Run the selected `--annotations` backends against a completed run's results.
///
/// Today only `check-run` is wired up. Posting failures are **non-fatal by
/// default**: the failure is logged as a warning and `Ok(())` is returned so the
/// caller keeps its content-driven exit code. With `strict` set, any posting
/// failure (including missing token / repo / head SHA) is returned as an `Err`,
/// which the caller propagates into a non-zero exit.
async fn emit_annotations(
    backends: &[AnnotationBackend],
    strict: bool,
    results: &[CheckResult],
    env: &CiEnvironment,
    vcs: &Vcs,
) -> Result<()> {
    if !backends.contains(&AnnotationBackend::CheckRun) {
        return Ok(());
    }

    if let Err(error) = post_check_run_annotations(results, env, vcs).await {
        if strict {
            return Err(
                error.context("failed to post check-run annotations (--annotations-strict is set, so this is fatal)")
            );
        }
        // Surface on stderr (not via tracing, whose default filter is off) so the
        // warning is visible without -v — matching `github_auth_unavailable_warning`.
        eprintln!(
            "warning: checkleft: could not post check-run annotations: {error:#}. Continuing — \
             the exit code reflects the checks themselves, not the posting failure. Pass \
             --annotations-strict to make this fatal."
        );
    }
    Ok(())
}

/// Resolve the GitHub context (owner/repo, head SHA, token) and POST the findings
/// as a check run. Each missing prerequisite is a distinct, actionable error so a
/// non-fatal warning (or strict failure) explains exactly what was absent.
async fn post_check_run_annotations(results: &[CheckResult], env: &CiEnvironment, vcs: &Vcs) -> Result<()> {
    let owner_repo = resolve_owner_repo(
        env,
        std::env::var(CHECKS_REPOSITORY_ENV).ok().as_deref(),
        vcs.remote_repo_slug().as_deref(),
    )
    .context(
        "could not resolve owner/repo (set GITHUB_REPOSITORY or CHECKS_REPOSITORY, or run inside \
         a git repository with an `origin` remote)",
    )?;

    let payload = env.read_github_event_payload();
    let head_sha = resolve_head_sha(env, payload.as_ref())
        .context("could not resolve the head commit SHA (no GitHub Actions or Buildkite commit env detected)")?;

    let token = detect_github_token().context(
        "no GitHub token found (checked CHECKS_GITHUB_TOKEN, GH_TOKEN, GITHUB_TOKEN and \
         `gh auth token`); a token with `Checks: write` is required to create a check run",
    )?;

    let base_url = github_api_base_url();
    let id = check_run::post_check_run(&base_url, &owner_repo, &token, &head_sha, results).await?;
    info!(check_run_id = id, owner_repo = %owner_repo, "posted checkleft check-run annotations");
    Ok(())
}

/// Base URL for GitHub REST API calls, honoring `GITHUB_API_URL` (set by GitHub
/// Actions, including on GitHub Enterprise Server) and otherwise defaulting to
/// public github.com.
fn github_api_base_url() -> String {
    std::env::var("GITHUB_API_URL")
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| check_run::GITHUB_API_BASE_URL.to_owned())
}

/// Per-check slice of a fix plan: which files the fix phase would process.
struct FixCheckPlan {
    check_id: String,
    /// Failing files (deduplicated, sorted) that would be passed to the fixer.
    failing_files: Vec<PathBuf>,
    /// Files excluded from fixing because they have uncommitted working-tree
    /// changes and `--allow_dirty=false` was set.
    dirty_skipped: Vec<PathBuf>,
}

/// Aggregated dry-run plan for `checkleft fix`: one entry per check with a
/// non-empty failing-file set after optional PATHS filtering.
struct FixPlan {
    checks: Vec<FixCheckPlan>,
}

/// Compute the fix plan from a completed run's results.
///
/// For each check, the failing-file set is the distinct `finding.location.path`
/// values whose severity is `Error` or `Warning` (Info findings are advisory
/// and are not fixed). If `paths` is non-empty the set is further intersected
/// with files that start with any of the given paths. Files in `dirty_paths`
/// are partitioned into `dirty_skipped` instead of `failing_files`; pass an
/// empty set when `--allow_dirty=true` (the default).
fn compute_fix_plan(results: &[CheckResult], paths: &[PathBuf], dirty_paths: &HashSet<PathBuf>) -> FixPlan {
    use std::collections::BTreeSet;

    let mut checks = Vec::new();
    for result in results {
        let failing: BTreeSet<PathBuf> = result
            .findings
            .iter()
            .filter(|f| matches!(f.severity, Severity::Error | Severity::Warning))
            .filter_map(|f| f.location.as_ref().map(|l| l.path.clone()))
            .collect();

        if failing.is_empty() {
            continue;
        }

        let path_filtered: Vec<PathBuf> = if paths.is_empty() {
            failing.into_iter().collect()
        } else {
            failing
                .into_iter()
                .filter(|file| paths.iter().any(|p| file.starts_with(p)))
                .collect()
        };

        if path_filtered.is_empty() {
            continue;
        }

        let mut failing_files = Vec::new();
        let mut dirty_skipped = Vec::new();
        for file in path_filtered {
            if dirty_paths.contains(&file) {
                dirty_skipped.push(file);
            } else {
                failing_files.push(file);
            }
        }

        if failing_files.is_empty() && dirty_skipped.is_empty() {
            continue;
        }

        checks.push(FixCheckPlan {
            check_id: result.check_id.clone(),
            failing_files,
            dirty_skipped,
        });
    }
    FixPlan { checks }
}

/// Outcome bucket for one check in the fix output.
enum FixCheckOutcome<'a> {
    /// Declarative fix was executed: each element is one invocation's outcome.
    Executed(&'a Vec<FixInvocationOutcome>),
    /// No fix block is declared for this check (no declarative `fix:` on any
    /// invocation, built-in, or WASM component).
    NoFixAvailable,
}

/// Per-check residual failure info after a re-verify pass.
///
/// Files are split by their worst-severity finding: a file with any Error
/// finding goes into `error_files`; a file with only Warning findings goes
/// into `warning_only_files`. Info-severity findings are excluded entirely.
/// This distinction drives the fix reporter — errors are blocking, warnings
/// are advisory and should not be styled as failures.
#[derive(Debug, Default)]
struct StillFailingInfo {
    /// Files with at least one Error-severity finding after re-verify.
    error_files: Vec<PathBuf>,
    /// Files with only Warning-severity findings after re-verify (non-blocking).
    warning_only_files: Vec<PathBuf>,
}

impl StillFailingInfo {
    fn contains_error(&self, path: &Path) -> bool {
        self.error_files.iter().any(|p| p.as_path() == path)
    }

    fn contains_warning_only(&self, path: &Path) -> bool {
        self.warning_only_files.iter().any(|p| p.as_path() == path)
    }
}

/// Build a per-check map of files still failing after a re-verify pass.
///
/// Files are split by worst severity: Error beats Warning. Info is excluded.
fn still_failing_from_verify(verify_results: &[CheckResult]) -> std::collections::BTreeMap<String, StillFailingInfo> {
    use std::collections::{BTreeMap, BTreeSet};

    // Track worst severity per (check_id, path) — Error beats Warning.
    let mut error_files: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();
    let mut warning_files: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();

    for r in verify_results {
        for f in &r.findings {
            let Some(location) = &f.location else { continue };
            match f.severity {
                Severity::Error => {
                    error_files
                        .entry(r.check_id.clone())
                        .or_default()
                        .insert(location.path.clone());
                    // Promote out of warning-only if it was there.
                    if let Some(wo) = warning_files.get_mut(&r.check_id) {
                        wo.remove(&location.path);
                    }
                }
                Severity::Warning => {
                    let already_error = error_files.get(&r.check_id).is_some_and(|s| s.contains(&location.path));
                    if !already_error {
                        warning_files
                            .entry(r.check_id.clone())
                            .or_default()
                            .insert(location.path.clone());
                    }
                }
                Severity::Info => {} // advisory — excluded from still-failing tracking
            }
        }
    }

    let all_check_ids: std::collections::BTreeSet<String> =
        error_files.keys().chain(warning_files.keys()).cloned().collect();
    all_check_ids
        .into_iter()
        .map(|check_id| {
            let info = StillFailingInfo {
                error_files: error_files
                    .get(&check_id)
                    .map(|s| s.iter().cloned().collect())
                    .unwrap_or_default(),
                warning_only_files: warning_files
                    .get(&check_id)
                    .map(|s| s.iter().cloned().collect())
                    .unwrap_or_default(),
            };
            (check_id, info)
        })
        .collect()
}

/// Collect the distinct set of applied files across all invocation outcomes for one check.
///
/// When multi-pass convergence is active, the same file may appear in `applied`
/// across several passes. This deduplicates them so the caller can report each
/// file once and count distinct files rather than summing per-pass occurrences.
fn distinct_applied_files(inv_outcomes: &[FixInvocationOutcome]) -> std::collections::BTreeSet<PathBuf> {
    inv_outcomes
        .iter()
        .flat_map(|inv| inv.applied.iter().cloned())
        .collect()
}

fn render_fix_results(
    plan: &FixPlan,
    outcomes: &std::collections::BTreeMap<String, Vec<FixInvocationOutcome>>,
    verify_results: Option<&[CheckResult]>,
    style: OutputStyle,
    elapsed: Duration,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    if plan.checks.is_empty() {
        let _ = writeln!(
            out,
            "{}: no failing files found (in {}s)\n",
            style.paint_info("fix"),
            elapsed.as_secs()
        );
        return out;
    }

    // Build a per-check map of files still failing after re-verify.
    let still_failing = verify_results.map(still_failing_from_verify).unwrap_or_default();

    let mut total_fixed = 0usize;
    let mut total_still_failing = 0usize;
    let mut total_warnings_remaining = 0usize;
    let mut total_errors = 0usize;
    let mut total_no_fix = 0usize;
    let total_dirty_skipped: usize = plan.checks.iter().map(|c| c.dirty_skipped.len()).sum();

    for check_plan in &plan.checks {
        let bucket = match outcomes.get(&check_plan.check_id) {
            Some(inv_outcomes) if !inv_outcomes.is_empty() => FixCheckOutcome::Executed(inv_outcomes),
            // Present but empty vec = no fix blocks declared (or non-declarative check).
            _ => FixCheckOutcome::NoFixAvailable,
        };

        let check_still_failing = still_failing.get(&check_plan.check_id);

        let _ = writeln!(out, "  {}:", style.paint_check_id(&check_plan.check_id));
        match bucket {
            FixCheckOutcome::NoFixAvailable => {
                let _ = writeln!(out, "    {}", style.paint_help_body("no fix available"));
                for file in &check_plan.failing_files {
                    let _ = writeln!(
                        out,
                        "    {} {}",
                        style.paint_help_body("  needs manual fix:"),
                        file.display()
                    );
                }
                total_no_fix += 1;
            }
            FixCheckOutcome::Executed(inv_outcomes) => {
                // Collect errors and distinct applied files across all passes.
                // Multi-pass convergence means the same file may appear in `applied`
                // for several passes (e.g. an oxfmt-non-idempotent file fixed in pass 1
                // and again in pass 2). Report each file once and suppress the
                // terminating no-op pass's "nothing to fix" line.
                let mut check_errors = 0usize;
                for inv in inv_outcomes {
                    for (file, msg) in &inv.per_file_errors {
                        let _ = writeln!(out, "    {} {}: {msg}", style.paint_error("error"), file.display());
                        total_errors += 1;
                        check_errors += 1;
                    }
                    if let Some(ref err) = inv.error {
                        let _ = writeln!(
                            out,
                            "    {}: {}",
                            style.paint_error("fix error"),
                            style.paint_message(&format!("[{}] {err:#}", inv.invocation_id))
                        );
                        total_errors += 1;
                        check_errors += 1;
                    }
                }

                let all_applied = distinct_applied_files(inv_outcomes);

                // Print applied files, split by post-verify status:
                //   - not in still_failing → fixed
                //   - in error_files → still failing (blocking)
                //   - in warning_only_files → warnings remain (non-blocking)
                for file in &all_applied {
                    let is_error = check_still_failing.is_some_and(|sf| sf.contains_error(file));
                    let is_warning_only = check_still_failing.is_some_and(|sf| sf.contains_warning_only(file));
                    if is_error {
                        let _ = writeln!(out, "    {} {}", style.paint_warning("still failing"), file.display());
                        total_still_failing += 1;
                    } else if is_warning_only {
                        let _ = writeln!(
                            out,
                            "    {} {}",
                            style.paint_warning("warning(s) remain (non-blocking)"),
                            file.display()
                        );
                        total_warnings_remaining += 1;
                    } else {
                        let _ = writeln!(out, "    {} {}", style.paint_info("fixed"), file.display());
                        total_fixed += 1;
                    }
                }

                // When the fixer applied nothing (no auto-fixable violations), print an
                // accurate message based on what re-verify found:
                //   - genuine no residue → "nothing to fix (already clean or no matching files)"
                //   - error residue → "no auto-fixable violations — N error(s) still failing (fix manually)"
                //   - warning-only residue → "N warning(s) remain (non-blocking)"
                if all_applied.is_empty() && check_errors == 0 {
                    let has_error_residue = check_still_failing.is_some_and(|sf| !sf.error_files.is_empty());
                    let has_warning_residue = check_still_failing.is_some_and(|sf| !sf.warning_only_files.is_empty());

                    if has_error_residue {
                        let n = check_still_failing.map(|sf| sf.error_files.len()).unwrap_or(0);
                        let _ = writeln!(
                            out,
                            "    {}",
                            style.paint_warning(&format!(
                                "no auto-fixable violations — {n} error(s) still failing (fix manually)"
                            ))
                        );
                        total_still_failing += n;
                    } else if has_warning_residue {
                        let n = check_still_failing.map(|sf| sf.warning_only_files.len()).unwrap_or(0);
                        let _ = writeln!(
                            out,
                            "    {}",
                            style.paint_warning(&format!("{n} warning(s) remain (non-blocking)"))
                        );
                        total_warnings_remaining += n;
                    } else {
                        let _ = writeln!(
                            out,
                            "    {}",
                            style.paint_help_body("nothing to fix (already clean or no matching files)")
                        );
                    }
                }

                // Files still failing for this check that appeared in verify but were
                // not in any invocation's applied set (edge case: a different check
                // on the same applied file that still fails). Distinguish error vs warning.
                // Only runs when the fixer applied at least one file; when nothing was
                // applied the block above already handled residue via aggregate messages.
                if !all_applied.is_empty()
                    && let Some(sf) = check_still_failing
                {
                    for file in &sf.error_files {
                        if !all_applied.contains(file) {
                            let _ = writeln!(out, "    {} {}", style.paint_warning("still failing"), file.display());
                            total_still_failing += 1;
                        }
                    }
                    for file in &sf.warning_only_files {
                        if !all_applied.contains(file) {
                            let _ = writeln!(
                                out,
                                "    {} {}",
                                style.paint_warning("warning(s) remain (non-blocking)"),
                                file.display()
                            );
                            total_warnings_remaining += 1;
                        }
                    }
                }
            }
        }
        for file in &check_plan.dirty_skipped {
            let _ = writeln!(out, "    {} (skipped: uncommitted changes)", file.display());
        }
        let _ = writeln!(out);
    }

    // Report checks from verify that were not in the fix plan (e.g. a different check
    // failing on a file that was fixed by another check).
    for (check_id, sf) in &still_failing {
        if plan.checks.iter().any(|c| &c.check_id == check_id) {
            continue; // already rendered above
        }
        let _ = writeln!(out, "  {} (also failing after fix):", style.paint_check_id(check_id));
        for file in &sf.error_files {
            let _ = writeln!(out, "    {} {}", style.paint_warning("still failing"), file.display());
            total_still_failing += 1;
        }
        for file in &sf.warning_only_files {
            let _ = writeln!(
                out,
                "    {} {}",
                style.paint_warning("warning(s) remain (non-blocking)"),
                file.display()
            );
            total_warnings_remaining += 1;
        }
        let _ = writeln!(out);
    }

    let verify_note = if verify_results.is_some() {
        String::new() // verify ran; still_failing and fixed counts are accurate
    } else {
        " (--verify=false; post-fix state unknown)".to_owned()
    };
    let dirty_note = if total_dirty_skipped > 0 {
        format!(", {total_dirty_skipped} skipped (dirty)")
    } else {
        String::new()
    };
    let warnings_note = if total_warnings_remaining > 0 {
        format!(", {total_warnings_remaining} warning(s) remaining (non-blocking)")
    } else {
        String::new()
    };
    let _ = writeln!(
        out,
        "{}: {} file(s) fixed, {} still failing, {} error(s), {} check(s) with no fix available{}{}{} (in {}s)\n",
        style.paint_bold("summary"),
        total_fixed,
        total_still_failing,
        total_errors,
        total_no_fix,
        warnings_note,
        dirty_note,
        verify_note,
        elapsed.as_secs()
    );

    out
}

fn print_fix_results(
    plan: &FixPlan,
    outcomes: &std::collections::BTreeMap<String, Vec<FixInvocationOutcome>>,
    verify_results: Option<&[CheckResult]>,
    style: OutputStyle,
    elapsed: Duration,
) {
    print!("{}", render_fix_results(plan, outcomes, verify_results, style, elapsed));
}

fn print_fix_results_json(
    plan: &FixPlan,
    outcomes: &std::collections::BTreeMap<String, Vec<FixInvocationOutcome>>,
    verify_results: Option<&[CheckResult]>,
) -> Result<()> {
    let still_failing = verify_results.map(still_failing_from_verify).unwrap_or_default();

    let checks: Vec<serde_json::Value> = plan
        .checks
        .iter()
        .map(|c| {
            let inv_outcomes = outcomes.get(&c.check_id);
            let status = match inv_outcomes {
                Some(v) if !v.is_empty() => "executed",
                _ => "no_fix_available",
            };
            let invocations: Vec<serde_json::Value> = inv_outcomes
                .map(|v| {
                    v.iter()
                        .map(|inv| {
                            serde_json::json!({
                                "invocation_id": inv.invocation_id,
                                "applied": inv.applied.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                                "per_file_errors": inv.per_file_errors.iter().map(|(p, m)| serde_json::json!({"file": p.display().to_string(), "error": m})).collect::<Vec<_>>(),
                                "error": inv.error.as_ref().map(|e| format!("{e:#}")),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let check_still_failing: Vec<String> = still_failing
                .get(&c.check_id)
                .map(|sf| {
                    sf.error_files.iter().chain(sf.warning_only_files.iter()).map(|p| p.display().to_string()).collect()
                })
                .unwrap_or_default();
            let distinct_applied: Vec<String> = inv_outcomes
                .map(|v| {
                    distinct_applied_files(v)
                        .into_iter()
                        .map(|p| p.display().to_string())
                        .collect()
                })
                .unwrap_or_default();
            serde_json::json!({
                "check_id": c.check_id,
                "failing_files": c.failing_files.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "dirty_skipped": c.dirty_skipped.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "fix_status": status,
                "invocations": invocations,
                "distinct_applied_files": distinct_applied,
                "still_failing_after_verify": check_still_failing,
            })
        })
        .collect();

    // Checks from verify not in the original plan (e.g. a different check failing
    // on a fixed file).
    let extra: Vec<serde_json::Value> = still_failing
        .iter()
        .filter(|(check_id, _)| !plan.checks.iter().any(|c| &c.check_id == *check_id))
        .map(|(check_id, sf)| {
            serde_json::json!({
                "check_id": check_id,
                "failing_files": [],
                "dirty_skipped": [],
                "fix_status": "no_fix_available",
                "invocations": [],
                "still_failing_after_verify": sf.error_files.iter().chain(sf.warning_only_files.iter()).map(|p| p.display().to_string()).collect::<Vec<_>>(),
            })
        })
        .collect();

    let mut output = checks;
    output.extend(extra);

    let wrapper = serde_json::json!({
        "verify_ran": verify_results.is_some(),
        "checks": output,
    });
    println!("{}", serde_json::to_string_pretty(&wrapper)?);
    Ok(())
}

async fn dispatch_fix(
    FixArgs {
        run_args:
            RunArgs {
                config,
                all,
                base_ref,
                default_branch,
                format,
                show_progress,
                annotations,
                annotations_out: _,
                annotations_strict: _,
                upload: _,
            },
        allow_dirty,
        verify,
        max_passes,
        paths,
    }: FixArgs,
    root: &Path,
    vcs: &Vcs,
    env: &CiEnvironment,
) -> Result<ExitCode> {
    let overrides = ChangeOverrides {
        all,
        base_ref,
        default_branch,
    };
    info!("resolving change plan for fix");
    let plan = resolve_change_plan(env, vcs, &overrides)?;
    info!("building runner for fix");
    let runner = build_runner(
        root,
        vcs,
        base_revision_from_plan(vcs, &plan),
        config.external_checks_file,
        config.external_checks_url,
    )
    .await?;
    info!("resolving changeset for fix");
    let changeset = attach_description_context(changeset_from_plan(vcs, &plan)?, vcs, env, &plan).await;
    info!(
        changed_files = changeset.changed_files.len(),
        "resolved changeset for fix"
    );

    let style = OutputStyle::detect_for_stdout();
    let progress_enabled = matches!(format, OutputFormat::Human)
        && should_show_progress(
            show_progress,
            style.level,
            std::io::stdout().is_terminal(),
            stderr().is_terminal(),
            detect_ci(),
        );
    let mut live = progress_enabled.then(|| LiveProgress::new(Box::new(TermRenderer::stdout()), DEFAULT_DEBOUNCE));
    let reporter: Arc<dyn ProgressReporter> = match &live {
        Some(progress) => progress.reporter(make_render_findings(style)),
        None => Arc::new(NoopProgressReporter),
    };

    let run_started_at = Instant::now();
    let mut results = runner
        .run_changeset_with_progress(&changeset, Arc::clone(&reporter))
        .await?;
    sort_results_for_output(&mut results);

    // When --allow_dirty=false, subtract files with uncommitted working-tree
    // changes from the fixable set so in-flight edits are never clobbered.
    let dirty_paths: HashSet<PathBuf> = if allow_dirty {
        HashSet::new()
    } else {
        vcs.dirty_paths().unwrap_or_default()
    };

    let fix_plan = compute_fix_plan(&results, &paths, &dirty_paths);

    // Build the per-check fix plan as a map for the runner.
    let fix_plan_map: std::collections::BTreeMap<String, Vec<PathBuf>> = fix_plan
        .checks
        .iter()
        .map(|c| (c.check_id.clone(), c.failing_files.clone()))
        .collect();

    // Execute declarative fix blocks inside T2 sandboxes; non-declarative checks
    // produce empty outcome vecs (no fix available). The same reporter is reused
    // for the apply phase so the LiveProgress instance covers both phases before
    // finalization.
    let mut fix_outcomes = runner.run_declarative_fixes(
        &changeset,
        &fix_plan_map,
        root,
        max_passes.unwrap_or(DEFAULT_FIX_PASSES),
        reporter,
    )?;

    // Apply suggested_fix edits from built-in check findings (T10). Only fills in
    // entries that run_declarative_fixes left empty (no declarative fix block); a
    // non-empty entry means the declarative path already ran for that check and
    // takes precedence.
    let builtin_outcomes = runner.apply_suggested_fixes(&results, &fix_plan_map, root);
    for (check_id, inv_outcomes) in builtin_outcomes {
        let entry = fix_outcomes.entry(check_id).or_default();
        if entry.is_empty() {
            *entry = inv_outcomes;
        }
    }

    // Collect the files that were atomically copied back to the real working tree.
    let applied_files: std::collections::BTreeSet<PathBuf> = fix_outcomes
        .values()
        .flat_map(|inv_outcomes| inv_outcomes.iter().flat_map(|inv| inv.applied.iter().cloned()))
        .collect();

    // Re-run checks over the applied files to detect any residual failures (§G).
    // Skipped when --verify=false or nothing was applied (no copy-back occurred).
    let verify_results: Option<Vec<CheckResult>> = if verify && !applied_files.is_empty() {
        info!(
            files = applied_files.len(),
            "re-running checks over fixed files (--verify)"
        );
        let verify_changeset = ChangeSet::new(
            applied_files
                .iter()
                .map(|path| ChangedFile {
                    path: path.clone(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                })
                .collect(),
        );
        let mut vr = runner
            .run_changeset_with_progress(&verify_changeset, Arc::new(NoopProgressReporter))
            .await?;
        sort_results_for_output(&mut vr);
        Some(vr)
    } else {
        None
    };

    // Finalize the progress UI now that both the discovery and apply phases are
    // complete. Any verify re-run above used NoopProgressReporter so the live
    // display did not overlap with the verify pass.
    if let Some(mut progress) = live.take() {
        progress.finalize();
    }

    let elapsed = run_started_at.elapsed();

    match format {
        OutputFormat::Human => print_fix_results(&fix_plan, &fix_outcomes, verify_results.as_deref(), style, elapsed),
        OutputFormat::Json => print_fix_results_json(&fix_plan, &fix_outcomes, verify_results.as_deref())?,
    }

    // Emit GHA workflow commands for residual findings (post-fix state).
    // Mirror the exit-code logic: when --verify ran, annotations come from
    // two sources — (a) verify_results for applied files (post-fix state),
    // and (b) original results filtered to files never applied (no fix was
    // available or the fixer failed). Merging both ensures the annotation set
    // matches the exit-code signal; omitting (b) would silently drop errors
    // that still contribute to exit code 1.
    // When --verify did not run (verify_results is None), fall back to the
    // original results (pre-fix state, the only available snapshot).
    if annotations.contains(&AnnotationBackend::Gha) {
        match verify_results.as_deref() {
            Some(vr) => {
                // Build a merged view: verify results for applied files + original
                // results for files that had no fix applied.
                let mut merged: Vec<CheckResult> = vr.to_vec();
                for r in &results {
                    let unresolved_findings: Vec<Finding> = r
                        .findings
                        .iter()
                        .filter(|f| {
                            f.location
                                .as_ref()
                                .map(|l| !applied_files.contains(&l.path))
                                .unwrap_or(true)
                        })
                        .cloned()
                        .collect();
                    if !unresolved_findings.is_empty() {
                        merged.push(CheckResult {
                            check_id: r.check_id.clone(),
                            findings: unresolved_findings,
                        });
                    }
                }
                emit_gha_annotations(&merged, env);
            }
            None => emit_gha_annotations(&results, env),
        }
    }

    // Exit 0 when no Error-severity finding remains after fixing and re-verifying (§A).
    // Fixer invocation errors count as operational Error findings regardless of verify mode.
    let fix_has_error = fix_outcomes
        .values()
        .flat_map(|invs| invs.iter())
        .any(|inv| inv.error.is_some() || !inv.per_file_errors.is_empty());

    let findings_error = match verify_results.as_deref() {
        Some(vr) => {
            // Applied files still failing after re-verify.
            let verify_error = vr
                .iter()
                .any(|r| r.findings.iter().any(|f| f.severity == Severity::Error));
            // Original errors on files that were never applied (no fix available or fixer failed).
            let unresolved_error = results.iter().any(|r| {
                r.findings.iter().any(|f| {
                    matches!(f.severity, Severity::Error)
                        && f.location
                            .as_ref()
                            .map(|l| !applied_files.contains(&l.path))
                            .unwrap_or(true)
                })
            });
            verify_error || unresolved_error
        }
        // Without re-verify: use original run results directly.
        None => results
            .iter()
            .any(|r| r.findings.iter().any(|f| f.severity == Severity::Error)),
    };

    Ok(if fix_has_error || findings_error {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// Collect annotations from all check results, apply the GHA per-step cap, and
/// print the workflow-command lines to stderr.
///
/// Stderr is used so that `--format=json` stdout output remains valid JSON;
/// the GitHub Actions runner intercepts workflow commands from both stdout and
/// stderr, so annotations still reach the UI either way.
///
/// Self-disables when `GITHUB_ACTIONS` is not `true`/`1` in the environment —
/// so `--annotations=gha` is safe to pass unconditionally in workflows that
/// also run locally; the lines are simply not printed on non-GHA platforms.
/// Force-enabling on non-GHA platforms is not currently implemented.
fn emit_gha_annotations(results: &[CheckResult], env: &CiEnvironment) {
    if !env.github_actions {
        return;
    }
    let raw: Vec<Annotation> = results
        .iter()
        .flat_map(|r| r.findings.iter().map(|f| (r.check_id.as_str(), f)))
        .filter_map(|(check_id, f)| annotation_from_finding(check_id, f))
        .collect();
    let capped = cap_gha_annotations(raw);
    eprint!("{}", format_gha_workflow_commands(&capped));
}

async fn build_runner(
    root: &Path,
    vcs: &Vcs,
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
    // Collect the VCS-tracked file set so that glob() can include files that
    // are committed but match a .gitignore pattern (tracked before the rule was
    // added), while still skipping untracked ignored build artifacts and deps.
    let tracked_paths = vcs
        .all_files_changeset()
        .unwrap_or_default()
        .changed_files
        .into_iter()
        .map(|f| f.path)
        .collect();
    let source_tree = Arc::new(LocalSourceTree::with_tracked_paths(root, base_revision, tracked_paths)?);
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
        // First-party defs embedded in the binary: zero install for target
        // repos. Grouped with the file provider (both are the non-generated
        // path), so `generated-only` mode excludes it too.
        providers.push(ConfiguredExternalCheckPackageProvider::new(
            "bundled",
            Arc::new(BundledExternalCheckPackageProvider),
        ));
    }

    let index_path = normalize_optional_description(std::env::var(CHECKLEFT_EXTERNAL_CHECK_INDEX_ENV).ok());
    if mode == ExternalProviderMode::GeneratedOnly && index_path.is_none() {
        anyhow::bail!(
            "`{CHECKLEFT_EXTERNAL_PROVIDER_MODE_ENV}=generated-only` requires `{CHECKLEFT_EXTERNAL_CHECK_INDEX_ENV}` to be set"
        );
    }
    if mode != ExternalProviderMode::FileOnly
        && let Some(index_path) = index_path
    {
        info!(index_path = %index_path, "loading generated external package index");
        let generated_provider = GeneratedExternalCheckPackageProvider::from_index_path(root, Path::new(&index_path))?;
        providers.push(ConfiguredExternalCheckPackageProvider::new(
            "generated-index",
            Arc::new(generated_provider),
        ));
    }

    if providers.is_empty() {
        return Ok(Arc::new(NoopExternalCheckPackageProvider));
    }
    Ok(Arc::new(CompositeExternalCheckPackageProvider::new(providers)))
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

async fn attach_description_context(
    changeset: ChangeSet,
    vcs: &Vcs,
    env: &CiEnvironment,
    plan: &ChangePlan,
) -> ChangeSet {
    info!("attaching commit and PR metadata");
    // When we know the base revision, read ALL commit descriptions in the pushed
    // range so that a BYPASS directive placed in any content commit (including
    // when @ is an empty working-copy commit on top of the real content) is seen.
    let commit_description = match plan {
        ChangePlan::Scoped { base_sha, .. } => {
            normalize_optional_description(vcs.commit_descriptions_since(base_sha).ok())
                .or_else(|| normalize_optional_description(vcs.current_commit_description().ok()))
        }
        _ => normalize_optional_description(vcs.current_commit_description().ok()),
    };
    let change_id = resolve_change_id(env);
    let repository = resolve_repository(vcs);
    let pr_description = normalize_optional_description(
        resolve_pr_description(repository.as_deref(), change_id.as_deref(), env, vcs).await,
    );
    changeset
        .with_commit_description(commit_description)
        .with_change_id(change_id)
        .with_repository(repository)
        .with_pr_description(pr_description)
}

/// Resolve the PR/change identifier used to fetch the PR description.
///
/// Fallback order (first non-empty value wins):
/// 1. Explicit CHECKS_CHANGE_ID / CHECKS_PR_NUMBER env vars.
/// 2. CI-native env: Buildkite's BUILDKITE_PULL_REQUEST (when not "false"),
///    GitHub Actions' GITHUB_REF parsed as refs/pull/{N}/merge.
///
/// Level 3 (branch→PR lookup via GitHub API) is handled inside
/// `resolve_pr_description` because it is async and may skip the PR-number
/// intermediary entirely.
fn resolve_change_id(env: &CiEnvironment) -> Option<String> {
    // Level 1: explicit CHECKS_* env (highest precedence)
    let explicit = [std::env::var(CHECKS_CHANGE_ID_ENV), std::env::var(CHECKS_PR_NUMBER_ENV)]
        .into_iter()
        .find_map(|v| normalize_optional_description(v.ok()));
    if explicit.is_some() {
        return explicit;
    }

    // Level 2a: Buildkite — BUILDKITE_PULL_REQUEST is the PR number on PR
    // builds, or the literal string "false" on push builds.
    if let Some(pr) = env.buildkite_pull_request.as_deref().filter(|v| *v != "false") {
        return normalize_optional_description(Some(pr.to_owned()));
    }

    // Level 2b: GitHub Actions — GITHUB_REF is "refs/pull/{N}/merge" or
    // "refs/pull/{N}/head" on pull_request events.
    if let Some(pr_number) = env.github_ref.as_deref().and_then(parse_github_ref_pr_number) {
        return Some(pr_number);
    }

    None
}

/// Extract a PR number string from a GitHub ref like "refs/pull/42/merge".
/// Returns None if the ref does not match the pull-request pattern.
fn parse_github_ref_pr_number(github_ref: &str) -> Option<String> {
    let after_prefix = github_ref.strip_prefix("refs/pull/")?;
    let number_str = after_prefix.split('/').next()?;
    // Validate it parses as a positive integer before returning.
    number_str.parse::<u64>().ok()?;
    Some(number_str.to_owned())
}

fn resolve_repository(vcs: &Vcs) -> Option<String> {
    normalize_optional_description(std::env::var(CHECKS_REPOSITORY_ENV).ok())
        .or_else(|| normalize_optional_description(vcs.remote_repo_slug()))
}

async fn resolve_pr_description(
    repository: Option<&str>,
    change_id: Option<&str>,
    env: &CiEnvironment,
    vcs: &Vcs,
) -> Option<String> {
    // Explicit override: highest precedence, no network call needed.
    if let Ok(raw) = std::env::var(CHECKS_PR_DESCRIPTION_ENV)
        && !raw.trim().is_empty()
    {
        return Some(raw);
    }

    let repository = repository?;
    let github_token = detect_github_token();

    if github_token.is_none() {
        eprintln!("{}", github_auth_unavailable_warning(repository));
    }

    // Levels 1 & 2: fetch description using the already-resolved change_id.
    if let Some(change_id) = change_id {
        info!(
            repository = repository,
            change_id = change_id,
            "fetching PR description by change id"
        );
        if let Some(desc) = github_pull_request_description(repository, change_id, github_token.as_deref()).await {
            return Some(desc);
        }
    }

    // Level 3: no PR number from env — detect the current branch and look up
    // the open PR via the GitHub API. Best-effort: missing token, no open PR,
    // or any network failure all silently yield None.
    let branch = detect_current_branch(env, vcs)?;
    info!(
        repository = repository,
        branch = branch,
        "resolving PR description via branch lookup"
    );
    let pr_number = github_pr_number_for_branch(repository, &branch, github_token.as_deref()).await?;
    info!(
        repository = repository,
        branch = branch,
        pr_number = pr_number,
        "fetching PR description for branch-resolved PR"
    );
    github_pull_request_description(repository, &pr_number, github_token.as_deref()).await
}

/// Detect the name of the current branch for Level 3 PR lookup.
///
/// Sources tried in order:
/// 1. Buildkite: `BUILDKITE_BRANCH` (always set, already the branch name).
/// 2. GitHub Actions: `GITHUB_HEAD_REF` (PR events) or `refs/heads/{branch}`
///    parsed from `GITHUB_REF` (push events).
/// 3. VCS fallback: `git branch --show-current` / jj bookmark.
fn detect_current_branch(env: &CiEnvironment, vcs: &Vcs) -> Option<String> {
    // Buildkite always exposes the branch; skip merge-queue synthetic branches.
    if let Some(branch) = env
        .buildkite_branch
        .as_deref()
        .filter(|b| !b.starts_with("gh-readonly-queue/"))
        .and_then(|b| normalize_optional_description(Some(b.to_owned())))
    {
        return Some(branch);
    }

    // GitHub Actions: GITHUB_HEAD_REF on pull_request events.
    if let Some(branch) = normalize_optional_description(env.github_head_ref.clone()) {
        return Some(branch);
    }

    // GitHub Actions: parse refs/heads/{branch} from GITHUB_REF on push events.
    if let Some(branch) = env
        .github_ref
        .as_deref()
        .and_then(|r| r.strip_prefix("refs/heads/"))
        .map(|b| b.trim().to_owned())
        .filter(|b| !b.is_empty())
    {
        return Some(branch);
    }

    // VCS fallback for local runs and any CI not covered above.
    normalize_optional_description(vcs.current_branch().ok())
}

fn init_tracing(verbose: u8, log_level: Option<LogLevel>, log_file: Option<PathBuf>) -> Result<()> {
    let filter = build_env_filter(verbose, log_level);
    let result = if let Some(path) = log_file {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open log file {}", path.display()))?;
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .try_init()
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(stderr)
            .try_init()
    };
    result.map_err(|err| anyhow!("failed to initialize tracing subscriber: {err}"))
}

/// Build an `EnvFilter` with the precedence: explicit `--log-level` > `-v` count > `RUST_LOG` > off.
fn build_env_filter(verbose: u8, log_level: Option<LogLevel>) -> EnvFilter {
    if let Some(level) = log_level {
        return EnvFilter::new(level.as_str());
    }
    if verbose > 0 {
        let level = match verbose {
            1 => "info",
            2 => "debug",
            _ => "trace",
        };
        return EnvFilter::new(level);
    }
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("off"))
}

/// Attempt a SARIF upload to GitHub code scanning. Non-fatal: missing context
/// (repository, commit SHA, token) and API errors are logged as warnings.
async fn attempt_sarif_upload(sarif: &serde_json::Value, env: &CiEnvironment, vcs: &Vcs) {
    let repository = match resolve_repository(vcs) {
        Some(r) => r,
        None => {
            eprintln!(
                "warning: checkleft: SARIF upload skipped — could not determine repository. \
                 Set CHECKS_REPOSITORY=owner/repo or ensure the git remote is configured."
            );
            return;
        }
    };

    let token = match detect_github_token() {
        Some(t) => t,
        None => {
            eprintln!(
                "warning: checkleft: SARIF upload skipped — no GitHub token found \
                 (checked CHECKS_GITHUB_TOKEN, GH_TOKEN, GITHUB_TOKEN env vars and `gh auth token`). \
                 A token with the `security_events` scope is required."
            );
            return;
        }
    };

    let event_payload = env.read_github_event_payload();
    let commit_sha = match resolve_head_sha(env, event_payload.as_ref()) {
        Some(sha) => sha,
        None => {
            eprintln!(
                "warning: checkleft: SARIF upload skipped — could not determine commit SHA. \
                 Set GITHUB_SHA (GHA) or BUILDKITE_COMMIT (Buildkite) to enable upload."
            );
            return;
        }
    };

    let git_ref = match resolve_ref_for_upload(env, vcs) {
        Some(r) => r,
        None => {
            eprintln!(
                "warning: checkleft: SARIF upload skipped — could not determine git ref. \
                 Set GITHUB_REF (GHA) or BUILDKITE_BRANCH (Buildkite) to enable upload."
            );
            return;
        }
    };

    let ctx = SarifUploadContext {
        repository: &repository,
        token: &token,
        commit_sha: &commit_sha,
        git_ref: &git_ref,
    };
    upload_sarif(sarif, &ctx).await;
}

/// Resolve the full git ref to attach to the SARIF upload.
///
/// The GitHub code scanning API requires a `ref` — ideally the same ref as the
/// CI event so findings appear inline in PRs. Resolution order (first match wins):
///
/// 1. GHA: `GITHUB_REF` (already a full ref: `refs/heads/...` or `refs/pull/.../merge`).
/// 2. Buildkite PR build: `BUILDKITE_PULL_REQUEST` → `refs/pull/{N}/merge`.
/// 3. Buildkite push build: `BUILDKITE_BRANCH` → `refs/heads/{branch}`.
/// 4. VCS fallback: current branch → `refs/heads/{branch}`.
fn resolve_ref_for_upload(env: &CiEnvironment, vcs: &Vcs) -> Option<String> {
    // GHA: GITHUB_REF is already a full ref.
    if let Some(github_ref) = env.github_ref.as_deref().filter(|r| !r.is_empty()) {
        return Some(github_ref.to_owned());
    }

    // Buildkite PR build: BUILDKITE_PULL_REQUEST is the PR number (not "false").
    if let Some(pr) = env
        .buildkite_pull_request
        .as_deref()
        .filter(|v| *v != "false" && !v.is_empty())
        && pr.parse::<u64>().is_ok()
    {
        return Some(format!("refs/pull/{pr}/merge"));
    }

    // Buildkite push build: BUILDKITE_BRANCH is the branch name.
    if let Some(branch) = env
        .buildkite_branch
        .as_deref()
        .filter(|b| !b.is_empty() && !b.starts_with("gh-readonly-queue/"))
    {
        return Some(format!("refs/heads/{branch}"));
    }

    // VCS fallback: current branch (local or any unrecognized CI).
    normalize_optional_description(vcs.current_branch().ok()).map(|b| format!("refs/heads/{b}"))
}

fn detect_github_token() -> Option<String> {
    resolve_github_token_from_sources(
        std::env::var(CHECKS_GITHUB_TOKEN_ENV).ok().as_deref(),
        std::env::var("GH_TOKEN").ok().as_deref(),
        std::env::var("GITHUB_TOKEN").ok().as_deref(),
        try_gh_auth_token().as_deref(),
    )
}

/// Attempt to obtain a GitHub token from the `gh` CLI (`gh auth token`).
/// Returns `None` if `gh` is not installed, not authenticated, or any error occurs.
/// Stderr from `gh` is suppressed — failures are handled silently.
fn try_gh_auth_token() -> Option<String> {
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        let raw = String::from_utf8(output.stdout).ok()?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    } else {
        None
    }
}

/// Resolve a GitHub token from the four possible sources, in priority order:
/// 1. `checks_github_token` — `CHECKS_GITHUB_TOKEN` env var (highest — explicit CI override)
/// 2. `gh_token` — `GH_TOKEN` env var
/// 3. `github_token` — `GITHUB_TOKEN` env var
/// 4. `gh_cli_token` — result of `gh auth token` (None when gh failed or is absent)
///
/// Accepts each source as an explicit parameter so the resolution logic is
/// testable without manipulating process environment variables.
fn resolve_github_token_from_sources(
    checks_github_token: Option<&str>,
    gh_token: Option<&str>,
    github_token: Option<&str>,
    gh_cli_token: Option<&str>,
) -> Option<String> {
    [checks_github_token, gh_token, github_token, gh_cli_token]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Build the warning message emitted when no GitHub auth is available but a
/// repository is known (so API calls would be attempted).
fn github_auth_unavailable_warning(repository: &str) -> String {
    format!(
        "warning: checkleft: PR-description bypass directives may be unavailable for {repository}: \
         no GitHub token found (checked CHECKS_GITHUB_TOKEN, GH_TOKEN, GITHUB_TOKEN env vars \
         and `gh auth token`). Run `gh auth login` or set a token env var to enable \
         authenticated GitHub API access."
    )
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

/// Decide whether to show the interactive progress UI.
///
/// An explicit `--show-progress=<bool>` always wins. Otherwise (auto) it is on
/// only for an interactive, color-capable terminal on both stdout and stderr and
/// outside CI — matching checkleft's existing color gating (`NO_COLOR`,
/// `is_terminal`) so the UI is never drawn into a pipe, a log, or CI output.
fn should_show_progress(flag: Option<bool>, color: ColorLevel, stdout_tty: bool, stderr_tty: bool, ci: bool) -> bool {
    match flag {
        Some(value) => value,
        None => color != ColorLevel::None && stdout_tty && stderr_tty && !ci,
    }
}

/// Whether we appear to be running under CI (where the progress UI is off by
/// default even if a pseudo-tty is allocated).
fn detect_ci() -> bool {
    ci_from_env(std::env::var("CI").ok().as_deref())
}

/// Pure core of [`detect_ci`]: a `CI` env value counts as CI when it is present
/// and not an explicit falsey value. Split out so it is testable without
/// mutating process environment.
fn ci_from_env(raw: Option<&str>) -> bool {
    matches!(raw, Some(value) if !value.is_empty() && value != "0" && value != "false")
}

/// Build the closure the progress reporter uses to render a check's findings
/// into the scrolling log area, in the same form as the non-interactive output.
fn make_render_findings(style: OutputStyle) -> RenderFindings {
    Arc::new(move |result: &CheckResult| {
        let mut findings = result.findings.clone();
        findings.sort_by_key(|finding| severity_sort_key(finding.severity));
        let mut out = String::new();
        for finding in &findings {
            out.push_str(&render_finding(result, finding, style));
        }
        out
    })
}

/// The trailing summary the interactive path prints after finalizing the status
/// block. The per-finding bodies already streamed into the log area, so for the
/// has-findings case this is only the summary line; the no-findings and
/// no-checks cases match [`render_human_results`] exactly.
fn render_human_footer(results: &[CheckResult], style: OutputStyle, elapsed: Duration) -> String {
    if results.is_empty() {
        return "No checks ran.\n".to_owned();
    }

    let total_findings: usize = results.iter().map(|result| result.findings.len()).sum();
    if total_findings == 0 {
        return format!(
            "{}: no findings ({} checks ran in {}s)\n",
            style.paint_info("checks"),
            results.len(),
            elapsed.as_secs()
        );
    }

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
        }
    }

    format!(
        "{}: {errors} error(s), {warnings} warning(s), {infos} info finding(s)\n",
        style.paint_bold("summary")
    )
}

fn render_human_results(results: &[CheckResult], style: OutputStyle, elapsed: Duration) -> String {
    if results.is_empty() {
        return "No checks ran.\n".to_owned();
    }

    let total_findings: usize = results.iter().map(|result| result.findings.len()).sum();
    if total_findings == 0 {
        return format!(
            "{}: no findings ({} checks ran in {}s)\n",
            style.paint_info("checks"),
            results.len(),
            elapsed.as_secs()
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
    let message = truncate_tool_output(&finding.message);
    out.push_str(&format!(
        "{}[{}]: {}\n",
        style.paint_severity(finding.severity),
        style.paint_check_id(&result.check_id),
        style.paint_message(&message)
    ));

    let location = finding
        .location
        .as_ref()
        .map(format_location)
        .unwrap_or_else(|| "<unknown>".to_owned());
    out.push_str(&format!("  --> {location}\n"));

    if !finding.remediations.is_empty() {
        if finding.remediations.len() > 1 {
            out.push_str(&format!("   = {}:\n", style.paint_help_label("to resolve")));
            let bullet = style.resolution_bullet();
            for item in &finding.remediations {
                out.push_str(&format!("   {bullet} {}\n", style.paint_help_body(item)));
            }
        } else {
            out.push_str(&format!(
                "   = {}: {}\n",
                style.paint_help_label("to resolve"),
                style.paint_help_body(&finding.remediations[0])
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

/// Maximum lines kept when truncating tool output for human display.
const TRUNCATE_MAX_LINES: usize = 5;
/// Maximum chars kept per line when truncating tool output for human display.
const TRUNCATE_MAX_LINE_LEN: usize = 200;
/// Maximum total chars kept across all lines when truncating tool output for human display.
const TRUNCATE_MAX_TOTAL_CHARS: usize = 1000;

/// Truncate potentially huge tool-error output for human display.
///
/// Caps to [`TRUNCATE_MAX_LINES`] lines, [`TRUNCATE_MAX_LINE_LEN`] chars per line,
/// and [`TRUNCATE_MAX_TOTAL_CHARS`] chars total. When anything is elided, appends a
/// marker like `… [output truncated: N more line(s), M more char(s)]`. Short/normal
/// messages return unchanged (Borrowed). Never called for JSON/structured output —
/// callers serialize CheckResult directly in that case.
fn truncate_tool_output(text: &str) -> std::borrow::Cow<'_, str> {
    let original_char_count: usize = text.chars().count();
    let original_line_count: usize = text.lines().count();

    let mut result_lines: Vec<String> = Vec::new();
    let mut kept_chars: usize = 0;
    let mut any_truncated = false;

    for line in text.lines() {
        if result_lines.len() >= TRUNCATE_MAX_LINES {
            any_truncated = true;
            break;
        }
        if kept_chars >= TRUNCATE_MAX_TOTAL_CHARS {
            any_truncated = true;
            break;
        }
        let remaining = TRUNCATE_MAX_TOTAL_CHARS - kept_chars;
        let line_chars: Vec<char> = line.chars().collect();
        let take = line_chars.len().min(TRUNCATE_MAX_LINE_LEN).min(remaining);
        if take < line_chars.len() {
            any_truncated = true;
            let clipped: String = line_chars[..take].iter().collect();
            kept_chars += take;
            result_lines.push(format!("{clipped}\u{2026}"));
        } else {
            kept_chars += line_chars.len();
            result_lines.push(line.to_owned());
        }
    }

    if !any_truncated {
        return std::borrow::Cow::Borrowed(text);
    }

    let lines_shown = result_lines.len();
    let more_lines = original_line_count.saturating_sub(lines_shown);
    let more_chars = original_char_count.saturating_sub(kept_chars);
    result_lines.push(format!(
        "\u{2026} [output truncated: {more_lines} more line(s), {more_chars} more char(s)]"
    ));

    std::borrow::Cow::Owned(result_lines.join("\n"))
}

fn format_fix_summary(suggested_fix: &SuggestedFix) -> String {
    format!(
        "{} ({} edit{})",
        suggested_fix.description,
        suggested_fix.edits.len(),
        if suggested_fix.edits.len() == 1 { "" } else { "s" }
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
        if no_color {
            return Self {
                level: ColorLevel::None,
            };
        }

        let clicolor_force = std::env::var("CLICOLOR_FORCE")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false);

        if !std::io::stdout().is_terminal() && !clicolor_force {
            return Self {
                level: ColorLevel::None,
            };
        }

        if let Ok(colorterm) = std::env::var("COLORTERM") {
            let ct = colorterm.to_ascii_lowercase();
            if ct == "truecolor" || ct == "24bit" {
                return Self {
                    level: ColorLevel::TrueColor,
                };
            }
        }

        if let Ok(term) = std::env::var("TERM")
            && term.contains("256color")
        {
            return Self {
                level: ColorLevel::Color256,
            };
        }

        Self {
            level: ColorLevel::Basic,
        }
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
