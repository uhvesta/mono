use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use console::{Style, style};
use git_utils::pr_bookmark;
use git_utils::repo_slug::{
    is_owner_name_slug, origin_path_matches_slug, origin_urls_equivalent, parse_github_remote,
    parse_org_name_shape,
};
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::audit;
use crate::cli::{
    ChangeCommand, Cli, Command, DoctorArgs, GraphArgs, PrCommand, PrEnsureArgs, PrPushArgs,
    RepoCommand, StackCommand, WorkspaceCommand,
};
use crate::command_runner::{CommandInvocation, CommandRunner, RealCommandRunner};
use crate::config;
use crate::lock::RepoLock;
use crate::metadata::{ChangeRecord, RepoRecord, WorkspaceHealth, WorkspaceRecord, WorkspaceState};
use crate::paths;
use crate::setup::{self, SetupReport, StepStatus, run_setup_engine};
use crate::store::{EffectiveState, Store, WorkspaceListFilter};

type Result<T> = std::result::Result<T, CubeError>;

/// Default lease TTL: 30 minutes from acquisition. The Boss-engine
/// integration sketch in R4 of v2-design-risks.md heartbeats every
/// few minutes against this window.
const DEFAULT_LEASE_TTL_SECS: i64 = 1800;

/// Pool-wide gc runs at most once per 24 hours, triggered from `cube workspace lease`.
const AUTO_GC_INTERVAL_SECS: i64 = 24 * 60 * 60;
const POOL_GC_LAST_AT_KEY: &str = "last_pool_gc_at";

#[derive(Debug, Clone)]
pub struct RunResult {
    pub message: String,
    pub payload: Value,
}

#[derive(Debug, Clone)]
struct RepoEnsureDefaults {
    repo_root: PathBuf,
    workspace_root: PathBuf,
}

#[derive(Debug, Clone)]
struct ChangeIdentity {
    jj_change_id: String,
    head_commit: String,
}

impl RunResult {
    fn new(message: impl Into<String>, payload: impl Serialize) -> Result<Self> {
        Ok(Self {
            message: message.into(),
            payload: serde_json::to_value(payload)?,
        })
    }
}

#[derive(Debug, Error)]
pub enum CubeError {
    #[error("{0}")]
    InvalidArgument(String),
    #[error("{0}")]
    NotImplemented(String),
    #[error("repo `{0}` is not configured")]
    RepoNotFound(String),
    #[error("no free workspace is available for repo `{0}`")]
    NoAvailableWorkspace(String),
    #[error("workspace `{0}` is not tracked")]
    WorkspaceNotFound(String),
    #[error("lease `{0}` is not tracked")]
    LeaseNotFound(String),
    #[error("change `{0}` is not tracked")]
    ChangeNotFound(String),
    #[error("setup step `{step}` failed: {error}")]
    SetupStepFailed { step: String, error: String },
    #[error("failed to access Cube metadata: {0}")]
    Storage(#[source] rusqlite::Error),
    #[error("failed to create workspace directory `{path}`: {source}")]
    WorkspaceDirCreate { path: PathBuf, source: io::Error },
    #[error("failed to read workspace directory `{path}`: {source}")]
    WorkspaceDirRead { path: PathBuf, source: io::Error },
    #[error("failed to remove workspace directory `{path}`: {source}")]
    WorkspaceDirRemove { path: PathBuf, source: io::Error },
    #[error("failed to create repo source directory `{path}`: {source}")]
    RepoSourceDirCreate { path: PathBuf, source: io::Error },
    #[error("failed to open state database at `{path}`: {source}")]
    StateDbIo { path: PathBuf, source: io::Error },
    #[error("failed to write audit log entry at `{path}`: {source}")]
    AuditLogIo { path: PathBuf, source: io::Error },
    #[error("failed to acquire repo lock at `{path}`: {source}")]
    LockIo { path: PathBuf, source: io::Error },
    #[error("I/O error: {0}")]
    Io(#[source] io::Error),
    #[error(
        "command `{program} {}` failed{}{}",
        args.join(" "),
        status
            .map(|code| format!(" with exit code {code}"))
            .unwrap_or_default(),
        if stderr.is_empty() {
            String::new()
        } else {
            format!(": {stderr}")
        }
    )]
    CommandFailed {
        program: String,
        args: Vec<String>,
        status: Option<i32>,
        stderr: String,
    },
    #[error("failed to serialize output: {0}")]
    Json(#[from] serde_json::Error),
    #[error("workspace `{workspace_path}` is stale and could not be auto-recovered: {cause}")]
    StaleRecoveryFailed {
        workspace_path: PathBuf,
        cause: String,
    },
    /// The lease handler tried to reclaim a workspace whose previous
    /// lease had expired (so cube flipped it back to `free`), but the
    /// workspace's `@` still has the prior holder's uncommitted /
    /// non-main work. A destructive `jj new <main>` would have silently
    /// destroyed it — most likely from underneath a worker whose lease
    /// expired but who is still active. Surface this loudly instead.
    /// Operators recover with `cube workspace force-release` after
    /// confirming the prior worker is genuinely gone.
    #[error(
        "workspace `{workspace_path}` was reclaimed from an expired lease (prior holder: {prior_holder}, \
         lease: {prior_lease_id}) but its working copy still has uncommitted work; refusing to \
         destructively reset it. Use `cube workspace force-release --lease {prior_lease_id}` to \
         acknowledge data loss and re-attempt the lease."
    )]
    LeaseExpiredWorkspaceDirty {
        workspace_path: PathBuf,
        prior_lease_id: String,
        prior_holder: String,
    },
}

/// Stable substring jj prints when a working copy is stale relative to
/// the shared op log. Verified against the version pinned in
/// `tools/jj/` — the wording has been stable across releases.
const JJ_STALE_SIGNATURE: &str = "working copy is stale";

/// Stable substring jj prints when the repo was loaded at an operation
/// that is a sibling of the working copy's operation (op-log divergence).
/// Both the stale-working-copy and op-log-diverged cases are fixed by
/// `jj workspace update-stale`. The wording has been stable across releases.
const JJ_OP_DIVERGED_SIGNATURE: &str = "seems to be a sibling";

/// Stable substring jj prints when a jj repo does not exist in the
/// current directory. If a `.git/` directory is present alongside the
/// missing `.jj/`, `jj git init --colocate` can recover the workspace.
const JJ_NO_JJ_REPO_SIGNATURE: &str = "there is no jj repo";

/// Stable substring jj prints from `jj bookmark track <name>@<remote>`
/// when the named remote bookmark does not exist in the repo (e.g.
/// asking it to track `main@origin` in a repo that uses `master`). Lets
/// cube swallow this specific failure during the post-clone "promote
/// the default branch" step without papering over other jj errors.
const JJ_NO_REMOTE_BOOKMARK_SIGNATURE: &str = "no such remote bookmark";

/// Stable substring jj prints when a revset references a revision that
/// does not exist — e.g. `jj bookmark set master -r master@origin` in a
/// workspace whose recorded default branch has no matching `@origin`
/// remote bookmark. Lets cube tolerate a misconfigured default branch
/// during the on-lease fast-forward without bricking the lease.
const JJ_REVISION_DOESNT_EXIST_SIGNATURE: &str = "doesn't exist";

impl CubeError {
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::InvalidArgument(_) | Self::NotImplemented(_) => ExitCode::from(2),
            Self::RepoNotFound(_) => ExitCode::from(3),
            Self::NoAvailableWorkspace(_) => ExitCode::from(4),
            Self::WorkspaceNotFound(_) | Self::LeaseNotFound(_) | Self::ChangeNotFound(_) => {
                ExitCode::from(5)
            }
            Self::SetupStepFailed { .. } => ExitCode::from(6),
            Self::Storage(_)
            | Self::Io(_)
            | Self::WorkspaceDirCreate { .. }
            | Self::WorkspaceDirRead { .. }
            | Self::WorkspaceDirRemove { .. }
            | Self::RepoSourceDirCreate { .. }
            | Self::StateDbIo { .. }
            | Self::AuditLogIo { .. }
            | Self::LockIo { .. }
            | Self::CommandFailed { .. }
            | Self::Json(_)
            | Self::StaleRecoveryFailed { .. } => ExitCode::FAILURE,
            // Surfaced as its own exit code so the engine's heartbeat
            // failure path can detect "I lost a lease and the workspace
            // still has work" specifically and surface it as a
            // `WorkAttentionItem` rather than a generic lease failure.
            Self::LeaseExpiredWorkspaceDirty { .. } => ExitCode::from(7),
        }
    }
}

pub fn run(cli: Cli) -> Result<RunResult> {
    let runner = RealCommandRunner;
    run_with_dependencies(cli, None, &runner)
}

fn run_with_dependencies(
    cli: Cli,
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
) -> Result<RunResult> {
    run_with_context(cli, database_path, runner, None, None)
}

fn run_with_context(
    cli: Cli,
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
    repo_ensure_defaults: Option<&RepoEnsureDefaults>,
    cube_config: Option<config::CubeConfig>,
) -> Result<RunResult> {
    match cli.command {
        Command::Repo { command } => {
            run_repo(command, database_path, runner, repo_ensure_defaults, cube_config)
        }
        Command::Workspace { command } => run_workspace(command, database_path, runner),
        Command::Change { command } => run_change(command, database_path, runner),
        Command::Stack { command } => run_stack(command),
        Command::Pr { command } => run_pr(command, runner),
        Command::Graph(args) => run_graph(args),
        Command::Doctor(args) => run_doctor(args),
    }
}

fn run_repo(
    command: RepoCommand,
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
    repo_ensure_defaults: Option<&RepoEnsureDefaults>,
    cube_config: Option<config::CubeConfig>,
) -> Result<RunResult> {
    let store = if let Some(path) = database_path {
        Store::open_at(path)?
    } else {
        Store::open_default()?
    };

    match command {
        RepoCommand::Ensure { reponame, origin } => {
            let defaults = if let Some(defaults) = repo_ensure_defaults {
                defaults.clone()
            } else {
                default_repo_ensure_defaults()?
            };
            let cfg = match cube_config {
                Some(c) => c,
                None => config::load_config()?,
            };
            let record = match (reponame, origin) {
                (_, Some(origin)) => {
                    // Explicit origin URL: skip name resolution and clone the
                    // URL directly with plain `jj git clone`.
                    let origin = normalize_origin(&origin)?;
                    let repo_id = repo_id_from_origin(&origin)?;
                    ensure_repo_core(&store, runner, &repo_id, &origin, None, &defaults)?
                }
                (Some(name), None) => {
                    ensure_repo_by_name(&store, runner, &name, &defaults, &cfg)?
                }
                (None, None) => {
                    // clap enforces that exactly one of the two is present.
                    return Err(CubeError::InvalidArgument(
                        "repo ensure requires a <reponame> or --origin <url>".to_string(),
                    ));
                }
            };
            let repo_id = record.repo.clone();
            RunResult::new(
                format!("Ensured repo `{repo_id}`."),
                json!({
                    "repo_id": repo_id,
                    "repo": record,
                }),
            )
        }
        RepoCommand::Add {
            repo,
            origin,
            main_branch,
            workspace_root,
            workspace_prefix,
            source,
        } => {
            let config = RepoRecord {
                repo,
                origin,
                main_branch,
                workspace_root: PathBuf::from(workspace_root),
                workspace_prefix,
                source: source.map(PathBuf::from),
                clone_command: None,
            };
            let record = store.upsert_repo(&config)?;
            RunResult::new(
                format!("Registered repo `{}`.", record.repo),
                json!({
                    "repo": record,
                }),
            )
        }
        RepoCommand::List => {
            let repos = store.list_repos()?;
            let message = format_repo_list(&repos);
            RunResult::new(
                message,
                json!({
                    "repos": repos,
                }),
            )
        }
        RepoCommand::Info { repo } => {
            let record = store
                .get_repo(&repo)?
                .ok_or_else(|| CubeError::RepoNotFound(repo.clone()))?;
            RunResult::new(
                human_repo_detail(&record),
                json!({
                    "repo": record,
                }),
            )
        }
    }
}

/// Resolve a bare `<reponame>` and ensure the repo, walking the resolution
/// chain in order; the first step that yields a URL wins:
///
///   1. **Existing slug.** A registered repo whose `slug == <reponame>` — the
///      slug *is* the reponame, so this is a no-op (idempotent re-ensure).
///   2. **Configured resolvers.** Each `repo-resolver` from cube's settings,
///      in declared order. The first whose `origin_pattern` produces a URL
///      wins; its optional `clone_command` materializes the repo.
///   3. **GitHub `<org>/<name>` fallback.** When `<reponame>` is in
///      `<org>/<name>` shape, synthesize `git@github.com:<org>/<name>.git`
///      and clone it with plain `jj git clone`.
///
/// When nothing produces a URL, the error names each step that was tried and
/// what it decided.
fn ensure_repo_by_name(
    store: &Store,
    runner: &dyn CommandRunner,
    name: &str,
    defaults: &RepoEnsureDefaults,
    cfg: &config::CubeConfig,
) -> Result<RepoRecord> {
    let name = name.trim();
    if name.is_empty() {
        return Err(CubeError::InvalidArgument(
            "repo name must not be empty".to_string(),
        ));
    }

    // Step 1: the reponame already names a registered slug.
    if let Some(existing) = store.get_repo(name)? {
        fs::create_dir_all(&existing.workspace_root).map_err(|e| {
            CubeError::WorkspaceDirCreate {
                path: existing.workspace_root.clone(),
                source: e,
            }
        })?;
        materialize_repo_source_if_missing(runner, &existing)?;
        return Ok(existing);
    }

    // Step 2: configured resolvers, in declared order.
    let mut resolver_notes: Vec<String> = Vec::new();
    for resolver in &cfg.repo_resolvers {
        match resolver.resolve_origin(name) {
            Some(origin) => {
                let clone_command = resolver.resolve_clone_command(name);
                // The reponame is the slug, so a later `cube repo ensure
                // <name>` short-circuits at step 1.
                return ensure_repo_core(store, runner, name, &origin, clone_command, defaults);
            }
            None => resolver_notes.push(format!(
                "resolver `{}`: pattern `{}` produced no URL",
                resolver.name, resolver.origin_pattern
            )),
        }
    }

    // Step 3: GitHub `<org>/<name>` fallback.
    if let Some((org, repo)) = parse_org_name_shape(name) {
        let origin = format!("git@github.com:{org}/{repo}.git");
        let repo_id = repo_id_from_origin(&origin)?;
        return ensure_repo_core(store, runner, &repo_id, &origin, None, defaults)
            .map_err(|err| github_fallback_error(err, &org, &repo));
    }

    let step2 = if cfg.repo_resolvers.is_empty() {
        "no `repo-resolvers` are configured".to_string()
    } else {
        resolver_notes.join("; ")
    };
    Err(CubeError::InvalidArgument(format!(
        "could not resolve repo `{name}`:\n  \
         1. registered slug: no repo with slug `{name}` exists\n  \
         2. resolvers: {step2}\n  \
         3. GitHub `<org>/<name>` fallback: `{name}` is not in `<org>/<name>` shape"
    )))
}

/// Register and materialize a repo given a fully-resolved origin and clone
/// strategy. `clone_command` (already `{name}`-substituted) is used in place
/// of `jj git clone` when present. Idempotent: an existing repo matched by
/// origin or by slug is reused rather than re-registered.
fn ensure_repo_core(
    store: &Store,
    runner: &dyn CommandRunner,
    repo_id: &str,
    origin: &str,
    clone_command: Option<String>,
    defaults: &RepoEnsureDefaults,
) -> Result<RepoRecord> {
    if let Some(record) = store.get_repo_by_origin(origin)? {
        fs::create_dir_all(&record.workspace_root).map_err(|e| CubeError::WorkspaceDirCreate {
            path: record.workspace_root.clone(),
            source: e,
        })?;
        materialize_repo_source_if_missing(runner, &record)?;
        return Ok(record);
    }

    let record = RepoRecord {
        repo: repo_id.to_string(),
        origin: origin.to_string(),
        main_branch: "main".to_string(),
        workspace_root: defaults.workspace_root.clone(),
        workspace_prefix: format!("{repo_id}-agent-"),
        source: Some(defaults.repo_root.join(repo_id)),
        clone_command,
    };
    if let Some(existing) = store.get_repo(&record.repo)? {
        // The repo is already configured under this id, so we never need to
        // synthesise an origin to clone with — `existing.origin` is the source
        // of truth. Two arrival shapes are acceptable:
        //
        //   1. An equivalent URL. URLs are treated as equivalent when they
        //      differ only in auth-identity prefix (e.g. `org-X@github.com:`
        //      vs `git@github.com:`) or trailing `.git`. Corporate git configs
        //      rewrite remotes with an org-specific user prefix, so the stored
        //      and incoming origins may not match exactly even when they point
        //      at the same repo.
        //
        //   2. A bare `owner/name` slug. Boss callers sometimes only carry the
        //      product's `owner/name` slug, not the registered origin URL.
        //      Rather than reconstruct an origin from the slug and assert on
        //      that guess (which can never match an SSO-scoped SSH origin like
        //      `org-127256988@github.com:...`), compare the slug against the
        //      *registered* origin's path and treat a match as a no-op success.
        let matches = origin_urls_equivalent(&existing.origin, origin)
            || (is_owner_name_slug(origin) && origin_path_matches_slug(&existing.origin, origin));
        if !matches {
            return Err(CubeError::InvalidArgument(format!(
                "repo `{}` is already configured for origin `{}`; cannot ensure `{origin}`",
                existing.repo, existing.origin
            )));
        }
        fs::create_dir_all(&existing.workspace_root).map_err(|e| CubeError::WorkspaceDirCreate {
            path: existing.workspace_root.clone(),
            source: e,
        })?;
        materialize_repo_source_if_missing(runner, &existing)?;
        return Ok(existing);
    }

    fs::create_dir_all(&record.workspace_root).map_err(|e| CubeError::WorkspaceDirCreate {
        path: record.workspace_root.clone(),
        source: e,
    })?;
    let detected_branch = materialize_repo_source_if_missing(runner, &record)?;
    let mut record = record;
    if let Some(branch) = detected_branch {
        if branch != record.main_branch {
            eprintln!(
                "cube: detected default branch `{branch}` for repo `{}`",
                record.repo
            );
        }
        record.main_branch = branch;
    }
    store.upsert_repo(&record)
}

/// Wrap a GitHub-fallback clone failure that looks like a missing remote with
/// guidance pointing at the resolver path. Other errors pass through unchanged.
fn github_fallback_error(err: CubeError, org: &str, repo: &str) -> CubeError {
    let looks_like_missing_remote = match &err {
        CubeError::CommandFailed { stderr, .. } => {
            let s = stderr.to_lowercase();
            s.contains("not found")
                || s.contains("does not exist")
                || s.contains("could not read from remote repository")
        }
        _ => false,
    };
    if looks_like_missing_remote {
        CubeError::InvalidArgument(format!(
            "fell back to GitHub `{org}/{repo}` — remote not found; if this is an \
             internal repo you may need a resolver. Add a `[[repo-resolvers]]` entry \
             to your cube config so `{repo}` resolves to the right origin."
        ))
    } else {
        err
    }
}

fn normalize_origin(origin: &str) -> Result<String> {
    let trimmed = origin.trim();
    if trimmed.is_empty() {
        return Err(CubeError::InvalidArgument(
            "origin must not be empty".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

fn default_repo_ensure_defaults() -> Result<RepoEnsureDefaults> {
    let cube_root = paths::data_dir()?;
    let repo_root = cube_root.join("repos");
    Ok(RepoEnsureDefaults {
        workspace_root: cube_root.join("workspaces"),
        repo_root,
    })
}

/// Clone the repo's source tree into `record.source` when it isn't present.
///
/// When `record.clone_command` is set (a resolver's `{name}`-substituted
/// command), that command is run in the workspace pool root in place of
/// `jj git clone` — it's expected to leave the working tree under
/// `<pool-root>/<reponame>`, after which cube colocates jj over it. Otherwise
/// cube runs `jj git clone <origin> <source>` and promotes the default branch.
fn materialize_repo_source_if_missing(
    runner: &dyn CommandRunner,
    record: &RepoRecord,
) -> Result<Option<String>> {
    let Some(source) = &record.source else {
        return Ok(None);
    };

    if source.exists() {
        if source.is_dir() {
            return Ok(None);
        }
        return Err(CubeError::InvalidArgument(format!(
            "source path {} exists and is not a directory",
            source.display()
        )));
    }

    let parent = source.parent().ok_or_else(|| {
        CubeError::InvalidArgument(format!(
            "cannot infer parent directory for source path {}",
            source.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(|e| CubeError::RepoSourceDirCreate {
        path: parent.to_path_buf(),
        source: e,
    })?;

    if let Some(clone_command) = &record.clone_command {
        let parts = shlex::split(clone_command).ok_or_else(|| {
            CubeError::InvalidArgument(format!(
                "resolver clone_command `{clone_command}` is not a parseable shell command"
            ))
        })?;
        let mut iter = parts.into_iter();
        let program = iter.next().ok_or_else(|| {
            CubeError::InvalidArgument(format!(
                "resolver clone_command `{clone_command}` is empty"
            ))
        })?;
        let args: Vec<String> = iter.collect();
        if which::which(&program).is_err() {
            return Err(CubeError::InvalidArgument(format!(
                "`{program}` (from resolver clone_command `{clone_command}`) is not on PATH; \
                 install it or fix the resolver in your cube config"
            )));
        }
        eprintln!(
            "cube: using `{clone_command}` to clone repo `{}`",
            record.repo
        );
        runner
            .run(&CommandInvocation {
                cwd: parent.to_path_buf(),
                program,
                args,
            })
            .map_err(|err| match err {
                CubeError::CommandFailed { stderr, .. } => CubeError::InvalidArgument(format!(
                    "resolver clone_command `{clone_command}` failed: {stderr}"
                )),
                other => other,
            })?;
        eprintln!(
            "cube: running `jj git init --colocate` in {}",
            source.display()
        );
        runner.run(&CommandInvocation {
            cwd: source.to_path_buf(),
            program: "jj".to_string(),
            args: vec![
                "git".to_string(),
                "init".to_string(),
                "--colocate".to_string(),
            ],
        })?;
        // The colocated clone already exposes the remote's branches as local
        // jj bookmarks, so there is nothing to promote here; we only need the
        // remote's default branch to record as the repo's `main_branch`.
        Ok(detect_remote_default_branch(runner, source, &record.origin))
    } else {
        eprintln!("cube: using `jj git clone` for repo `{}`", record.repo);
        // Detect the remote's default branch up front so we can both track the
        // right bookmark below and record it as the repo's `main_branch`.
        let default_branch = detect_remote_default_branch(runner, parent, &record.origin);
        runner.run(&CommandInvocation {
            cwd: parent.to_path_buf(),
            program: "jj".to_string(),
            args: vec![
                "git".to_string(),
                "clone".to_string(),
                record.origin.clone(),
                source.display().to_string(),
            ],
        })?;
        track_remote_bookmarks(runner, source, default_branch.as_deref())?;
        Ok(default_branch)
    }
}

/// Best-effort detection of the remote's default (integration) branch via
/// `git ls-remote --symref <origin> HEAD`, which reports the symbolic ref that
/// `HEAD` points at without needing the repo cloned first. Returns the short
/// branch name (e.g. `main`, `master`, `develop`) or `None` when detection
/// fails for any reason — `git` missing, network/auth failure, or unparseable
/// output — so callers fall back to the historical `main` default rather than
/// hard-failing materialization. SSH-prefixed origins (`org-N@github.com:...`)
/// authenticate via SSH key here, so corporate SSO does not block detection.
fn detect_remote_default_branch(
    runner: &dyn CommandRunner,
    cwd: &Path,
    origin: &str,
) -> Option<String> {
    let output = runner
        .run(&CommandInvocation {
            cwd: cwd.to_path_buf(),
            program: "git".to_string(),
            args: vec![
                "ls-remote".to_string(),
                "--symref".to_string(),
                origin.to_string(),
                "HEAD".to_string(),
            ],
        })
        .ok()?;
    parse_symref_default_branch(&output)
}

/// Parse the branch name out of `git ls-remote --symref` output. The relevant
/// line looks like `ref: refs/heads/<branch>\tHEAD`; the trailing `<sha>\tHEAD`
/// line and any warnings are ignored. Returns `None` when no such line is
/// present.
fn parse_symref_default_branch(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("ref:")?.trim_start();
        let rest = rest.strip_prefix("refs/heads/")?;
        let branch = rest.split_whitespace().next()?;
        (!branch.is_empty()).then(|| branch.to_string())
    })
}

/// Promote `main@origin` and `master@origin` to local tracking
/// bookmarks. `jj git clone` only creates remote bookmarks, so a fresh
/// clone has no local `main`/`master` for the lease's `jj new <main>`
/// step to resolve. We deliberately track only these two default-branch
/// names rather than every `*@origin` ref — large repos can carry
/// hundreds of long-lived feature/release/`gh-readonly-queue/*` refs
/// that would otherwise pollute the local bookmark namespace and slow
/// down `jj log` / `jj bookmark list` in every leased workspace.
///
/// "No such remote bookmark" is tolerated per-branch (most repos use
/// either `main` or `master`, not both). Other errors from `jj` are
/// propagated so a broken jj install, network failure mid-clone, or
/// permission error doesn't get silently swallowed. If neither bookmark
/// exists at all, the clone is unusable for cube's lease flow and we
/// surface a hard error rather than letting the caller stumble into
/// `jj new <missing>` later. Idempotent: re-tracking an already-tracked
/// bookmark is a no-op.
fn track_remote_bookmarks(
    runner: &dyn CommandRunner,
    repo_path: &Path,
    default_branch: Option<&str>,
) -> Result<()> {
    // Always attempt the two conventional defaults; additionally attempt the
    // detected default branch when it is something else (e.g. `develop`,
    // `trunk`) so the lease's later `jj new <main_branch>` has a local bookmark
    // to resolve. Keeping `main`/`master` first preserves the historical
    // tracking order for the common cases.
    let mut candidates: Vec<String> = vec!["main".to_string(), "master".to_string()];
    if let Some(branch) = default_branch
        && !candidates.iter().any(|c| c == branch) {
            candidates.push(branch.to_string());
        }
    let mut tracked_any = false;
    for branch in &candidates {
        let result = runner.run(&CommandInvocation {
            cwd: repo_path.to_path_buf(),
            program: "jj".to_string(),
            args: vec![
                "bookmark".to_string(),
                "track".to_string(),
                format!("{branch}@origin"),
            ],
        });
        match result {
            Ok(_) => tracked_any = true,
            Err(err) if is_no_such_remote_bookmark(&err) => {}
            Err(err) => return Err(err),
        }
    }
    if !tracked_any {
        let names = candidates
            .iter()
            .map(|b| format!("`{b}@origin`"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(CubeError::SetupStepFailed {
            step: "track_remote_bookmarks".to_string(),
            error: format!(
                "fresh clone at `{}` has none of {names}; \
                 cube cannot promote a default branch to local tracking",
                repo_path.display()
            ),
        });
    }
    Ok(())
}

/// Returns `true` when the error is `jj bookmark track`'s "no such
/// remote bookmark" diagnostic — meaning the named `<branch>@origin`
/// does not exist in this freshly-cloned repo. Distinct from "jj is
/// broken / clone hasn't finished / network died" failures, which must
/// propagate so callers don't silently misinterpret them as the repo
/// simply not using that default-branch name.
fn is_no_such_remote_bookmark(err: &CubeError) -> bool {
    let CubeError::CommandFailed { program, stderr, .. } = err else {
        return false;
    };
    if program != "jj" {
        return false;
    }
    stderr
        .to_lowercase()
        .contains(JJ_NO_REMOTE_BOOKMARK_SIGNATURE)
}

/// Returns `true` when `err` is jj reporting that the on-lease
/// fast-forward target (`<main>@origin`) could not be resolved — either
/// the "no such remote bookmark" wording or the revset "doesn't exist"
/// wording, depending on jj version/command. Lets the fast-forward step
/// degrade to a warning (and keep the prior local bookmark) for a repo
/// whose recorded default branch has no matching remote bookmark,
/// instead of failing the whole lease.
fn is_unresolved_remote_target(err: &CubeError) -> bool {
    let CubeError::CommandFailed { program, stderr, .. } = err else {
        return false;
    };
    if program != "jj" {
        return false;
    }
    let lower = stderr.to_lowercase();
    lower.contains(JJ_NO_REMOTE_BOOKMARK_SIGNATURE)
        || lower.contains(JJ_REVISION_DOESNT_EXIST_SIGNATURE)
}

fn repo_id_from_origin(origin: &str) -> Result<String> {
    let trimmed = origin.trim().trim_end_matches('/');
    let tail = trimmed
        .rsplit(|ch| ['/', ':'].contains(&ch))
        .next()
        .unwrap_or("");
    let tail = tail.strip_suffix(".git").unwrap_or(tail);
    let repo = sanitize_repo_id(tail);
    if repo.is_empty() {
        return Err(CubeError::InvalidArgument(format!(
            "could not infer repo id from origin `{origin}`"
        )));
    }
    Ok(repo)
}

fn sanitize_repo_id(raw: &str) -> String {
    let mut repo = String::new();
    let mut previous_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            repo.push(ch.to_ascii_lowercase());
            previous_dash = false;
            continue;
        }

        if matches!(ch, '-' | '_' | '.') && !previous_dash {
            repo.push('-');
            previous_dash = true;
        }
    }

    repo.trim_matches('-').to_string()
}

fn run_workspace(
    command: WorkspaceCommand,
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
) -> Result<RunResult> {
    let mut store = if let Some(path) = database_path {
        Store::open_at(path)?
    } else {
        Store::open_default()?
    };

    match command {
        WorkspaceCommand::Lease {
            repo,
            task,
            prefer,
            allow_dirty,
            resume_pr,
        } => {
            let repo_record = store
                .get_repo(&repo)?
                .ok_or_else(|| CubeError::RepoNotFound(repo.clone()))?;

            let leased_at_epoch_s = current_epoch_s()?;
            // Kick a background pool-wide gc pass at most once per 24h.
            maybe_trigger_pool_gc(&mut store, database_path, leased_at_epoch_s)?;

            let _lock = RepoLock::acquire(&repo_lock_path(&repo, database_path)?)?;
            let mut candidates = discover_workspaces(&repo_record)?;
            store.sync_workspaces(&repo, &candidates)?;
            // Sweep any leases that have already exceeded their TTL so they
            // become claimable again. Audit each reclaimed lease before
            // doing anything else so the timeline shows the prior
            // holder/task — when a worker's `@` is observed to move
            // mid-flight, this is the first thing to grep.
            let expired = store.expire_stale_leases(&repo, leased_at_epoch_s)?;
            for swept in &expired {
                audit!(
                    database_path,
                    "lease.expired_reclaimed",
                    repo = repo,
                    workspace_id = swept.workspace_id,
                    prior_lease_id = swept.lease_id,
                    prior_holder = swept.holder.as_deref(),
                    prior_task = swept.task.as_deref(),
                    prior_leased_at_epoch_s = swept.leased_at_epoch_s,
                    prior_lease_expires_at_epoch_s = swept.lease_expires_at_epoch_s,
                );
            }
            let expired_by_workspace_id: std::collections::HashMap<&str, &crate::store::ExpiredLease> =
                expired
                    .iter()
                    .map(|e| (e.workspace_id.as_str(), e))
                    .collect();
            // Self-heal any rows whose on-disk directory has been deleted
            // out from under cube. The repo lock is already held by this
            // lease call, so use the `_in_repo` variant that skips its own
            // locking. After expire_stale_leases above, any leased rows
            // whose lease has aged out are now `free`, and reconcile will
            // forget them too if their directory is also missing.
            reconcile_missing_workspaces_in_repo(
                &mut store,
                database_path,
                &repo,
                leased_at_epoch_s,
            )?;

            let lease_id = Uuid::new_v4().to_string();
            let holder = holder_identity();
            let lease_expires_at = Some(leased_at_epoch_s + DEFAULT_LEASE_TTL_SECS);

            // ── Dirty-recovery phase ────────────────────────────────────────
            // `--allow-dirty` (always paired with `--prefer`, enforced by
            // clap) reclaims the named workspace *with its working copy
            // intact* so a recovery re-dispatch can land back on the only
            // copy of a crashed worker's unpushed work. This deliberately
            // bypasses everything the normal path does to a dirty
            // workspace — it is NOT health-skipped, and the destructive
            // `jj git fetch && jj new main` reset is suppressed so the
            // uncommitted tree survives the lease.
            //
            // Unlike best-effort `--prefer`, this never silently falls
            // back to a fresh workspace: routing the recovering worker
            // away from the dirty tree is exactly the bug this flag
            // exists to fix (spinyfin/mono#963). If the named workspace
            // cannot be reclaimed as-is, fail loudly.
            if allow_dirty {
                let pref = prefer
                    .as_deref()
                    .expect("clap enforces --prefer when --allow-dirty is set");

                let target = store
                    .list_workspaces_filtered(&WorkspaceListFilter {
                        repo: Some(&repo),
                        workspace_id: Some(pref),
                        ..Default::default()
                    })?
                    .into_iter()
                    .next()
                    .ok_or_else(|| CubeError::WorkspaceNotFound(pref.to_string()))?;

                if target.state == WorkspaceState::Leased {
                    // Held by a live worker (or an unexpired lease). We must
                    // not stomp on it; the operator should force-release
                    // first if the holder is genuinely gone.
                    return Err(CubeError::InvalidArgument(format!(
                        "workspace `{pref}` is currently leased; cannot reclaim it dirty. \
                         Use `cube workspace force-release` first if the prior holder is gone."
                    )));
                }

                if !workspace_path_exists(&target) {
                    return Err(CubeError::WorkspaceNotFound(pref.to_string()));
                }
                // A directory with neither .jj/ nor .git/ holds no
                // recoverable work — there is nothing dirty to reclaim.
                if !target.workspace_path.join(".jj").is_dir()
                    && !target.workspace_path.join(".git").is_dir()
                {
                    return Err(CubeError::InvalidArgument(format!(
                        "workspace `{pref}` has neither a .jj nor a .git directory; \
                         there is no in-flight work to reclaim with --allow-dirty."
                    )));
                }

                let mut workspace = store
                    .claim_specific_workspace(
                        &repo,
                        pref,
                        &holder,
                        &task,
                        &lease_id,
                        leased_at_epoch_s,
                        lease_expires_at,
                    )?
                    .ok_or_else(|| CubeError::NoAvailableWorkspace(repo.clone()))?;

                // No reset: the working copy is handed over exactly as the
                // prior holder left it. Record whatever `@` currently is so
                // the registry's head_commit reflects the recovered state.
                let head_commit =
                    current_workspace_commit(runner, database_path, &workspace.workspace_path)?;
                store.update_workspace_head_commit(&lease_id, Some(&head_commit))?;
                workspace.head_commit = Some(head_commit);

                audit!(
                    database_path,
                    "lease.acquired_dirty",
                    repo = workspace.repo,
                    workspace_id = workspace.workspace_id,
                    lease_id = lease_id,
                    holder = holder,
                    task = task,
                    head_commit = workspace.head_commit,
                );

                let setup_report = run_setup_for_workspace(&store, runner, &workspace)?;

                let lease_message = format!(
                    "Reclaimed {} (dirty) at {}.",
                    workspace.workspace_id,
                    workspace.workspace_path.display()
                );

                if let Some(failure) = setup_report.first_failure() {
                    let StepStatus::Failed { error } = &failure.status else {
                        unreachable!("first_failure returned non-failure step");
                    };
                    return Err(CubeError::SetupStepFailed {
                        step: failure.id.clone(),
                        error: error.clone(),
                    });
                }

                let message = format_lease_message(&lease_message, &setup_report);
                return RunResult::new(
                    message,
                    json!({
                        "workspace": workspace,
                        "setup": setup_report,
                        "health_check": [json!({
                            "workspace_id": pref,
                            "allow_dirty": true,
                            "reset_skipped": true,
                        })],
                    }),
                );
            }

            // ── Health-check phase ──────────────────────────────────────────
            // Before claiming any workspace, inspect each free candidate:
            //   - Clean → use immediately
            //   - ConflictedBookmarks → save as first repairable candidate
            //     (keep looking for a clean one; repair before claim)
            //   - DirtyWorkingCopy → skip and mark in the store so
            //     `cube workspace list` surfaces it
            //
            // The repo lock is held throughout, so no concurrent lease can
            // steal a workspace between the health check and the claim.

            let free_workspaces = store.list_workspaces_filtered(&WorkspaceListFilter {
                repo: Some(&repo),
                effective_state: Some(EffectiveState::Free),
                ..Default::default()
            })?;

            // Ordering: try the --prefer workspace first, then others by id.
            let ordered_ids: Vec<String> = {
                let mut v = Vec::new();
                if let Some(pref) = prefer.as_deref()
                    && free_workspaces.iter().any(|w| w.workspace_id == pref) {
                        v.push(pref.to_string());
                    }
                for w in &free_workspaces {
                    if !v.contains(&w.workspace_id) {
                        v.push(w.workspace_id.clone());
                    }
                }
                v
            };

            let mut health_checks: Vec<serde_json::Value> = Vec::new();
            // first clean workspace found
            let mut clean_candidate: Option<String> = None;
            // first conflicted-but-repairable workspace found
            let mut conflicted_candidate: Option<(String, Vec<String>)> = None;
            // Free workspaces whose directory exists but has neither .jj/ nor
            // .git/ — husks holding no recoverable work. Collected as
            // (workspace_id, path) so the lease path can GC them and reuse the
            // freed slot rather than surfacing a "broken-empty" failure.
            let mut broken_empty: Vec<(String, PathBuf)> = Vec::new();

            for ws_id in &ordered_ids {
                let ws = free_workspaces
                    .iter()
                    .find(|w| w.workspace_id == *ws_id)
                    .expect("ordered_ids came from free_workspaces");

                if !workspace_path_exists(ws) {
                    // Will be reconciled; skip for health-check purposes.
                    health_checks.push(json!({
                        "workspace_id": ws_id,
                        "skipped": true,
                        "reason": "directory_missing",
                    }));
                    continue;
                }

                let outcome = check_workspace_health(runner, database_path, &ws.workspace_path)?;
                match outcome {
                    WorkspaceHealthOutcome::Clean => {
                        health_checks.push(json!({
                            "workspace_id": ws_id,
                            "health": "clean",
                            "skipped": false,
                        }));
                        clean_candidate = Some(ws_id.clone());
                        break;
                    }
                    WorkspaceHealthOutcome::ConflictedBookmarks(ref bookmarks) => {
                        health_checks.push(json!({
                            "workspace_id": ws_id,
                            "health": "conflicted",
                            "bookmarks": bookmarks,
                            "skipped": conflicted_candidate.is_some(),
                        }));
                        store.update_workspace_health(
                            &repo,
                            ws_id,
                            WorkspaceHealth::Conflicted,
                        )?;
                        if conflicted_candidate.is_none() {
                            conflicted_candidate =
                                Some((ws_id.clone(), bookmarks.clone()));
                        }
                        // Keep looking for a clean one before falling back
                        // to repairing the conflicted one.
                    }
                    WorkspaceHealthOutcome::DirtyWorkingCopy => {
                        health_checks.push(json!({
                            "workspace_id": ws_id,
                            "health": "dirty",
                            "skipped": true,
                            "reason": "dirty_working_copy",
                        }));
                        store.update_workspace_health(
                            &repo,
                            ws_id,
                            WorkspaceHealth::Dirty,
                        )?;
                        audit!(
                            database_path,
                            "workspace.health_check_skipped",
                            repo = repo,
                            workspace_id = ws_id,
                            reason = "dirty_working_copy",
                        );
                    }
                    WorkspaceHealthOutcome::BrokenEmpty => {
                        let ws_path_str = ws.workspace_path.display().to_string();
                        health_checks.push(json!({
                            "workspace_id": ws_id,
                            "workspace_path": ws_path_str,
                            "health": "broken_empty",
                            "has_git": false,
                            "has_jj": false,
                            "skipped": true,
                            "reason": "neither_git_nor_jj",
                        }));
                        broken_empty.push((ws_id.clone(), ws.workspace_path.clone()));
                        audit!(
                            database_path,
                            "workspace.broken_empty",
                            repo = repo,
                            workspace_id = ws_id,
                            workspace_path = ws_path_str,
                        );
                    }
                }
            }

            // Decide which workspace to use: prefer clean, fall back to the
            // first repairable conflicted workspace, otherwise auto-create a
            // fresh one. No pool state (dirty, husk, occupied) is ever a hard
            // stop — cube always provisions new capacity for a reachable repo.
            let chosen_id = clean_candidate.or_else(|| {
                conflicted_candidate.as_ref().map(|(id, _)| id.clone())
            });

            let (mut workspace, was_auto_created, repair_bookmarks) = if let Some(ws_id) =
                chosen_id
            {
                // Claim the specific workspace we health-checked.
                let ws = store
                    .claim_specific_workspace(
                        &repo,
                        &ws_id,
                        &holder,
                        &task,
                        &lease_id,
                        leased_at_epoch_s,
                        lease_expires_at,
                    )?
                    .ok_or_else(|| CubeError::NoAvailableWorkspace(repo.clone()))?;
                let bookmarks = conflicted_candidate
                    .filter(|(id, _)| *id == ws_id)
                    .map(|(_, b)| b)
                    .unwrap_or_default();
                (ws, false, bookmarks)
            } else {
                // No clean or repairable workspace. Self-heal any broken-empty
                // husks first: a directory with neither .jj/ nor .git/ holds no
                // recoverable work, so delete it and free its slot instead of
                // surfacing it to the caller. A husk must never be a reason to
                // deny a lease for a reachable repo (issue #845 part 2b).
                for (ws_id, ws_path) in &broken_empty {
                    gc_broken_empty_workspace(&mut store, database_path, &repo, ws_id, ws_path)?;
                    candidates.retain(|c| &c.workspace_id != ws_id);
                }

                // Pool is empty, fully leased, only had broken-empty husks
                // (now GC'd), or all free slots are dirty: grow the pool by
                // one. Dirty workspaces are left untouched so their unpushed
                // work is preserved for later inspection. The pool is an
                // optimisation (reuse a known-good checkout), never a hard
                // cap — a lease for a reachable repo always succeeds.
                let new_candidate = auto_create_workspace(runner, &repo_record, &candidates)?;
                let new_id = new_candidate.workspace_id.clone();
                candidates.push(new_candidate);
                store.sync_workspaces(&repo, &candidates)?;
                // Claim the workspace we just created by id. A generic claim
                // could otherwise grab a leftover free-but-dirty workspace and
                // destructively reset it — the dirty entries are intentionally
                // preserved for the operator.
                let ws = store
                    .claim_specific_workspace(
                        &repo,
                        &new_id,
                        &holder,
                        &task,
                        &lease_id,
                        leased_at_epoch_s,
                        lease_expires_at,
                    )?
                    .ok_or_else(|| CubeError::NoAvailableWorkspace(repo.clone()))?;
                (ws, true, vec![])
            };

            if !workspace_path_exists(&workspace) {
                // Reconcile above should have caught this on the pre-claim
                // pass, but a concurrent `rm -rf` between reconcile and
                // claim can still land here. Drop the row and surface a
                // warning so the operator sees what happened.
                eprintln!(
                    "warning: cube workspace `{}/{}` directory disappeared between reconcile \
                     and claim at {}; dropping the dangling registry row",
                    workspace.repo,
                    workspace.workspace_id,
                    workspace.workspace_path.display(),
                );
                audit!(
                    database_path,
                    "workspace.dir_missing_reconciled",
                    repo = workspace.repo,
                    workspace_id = workspace.workspace_id,
                    workspace_path = workspace.workspace_path.display().to_string(),
                    prior_state = workspace.state.as_str(),
                    lease_id = lease_id,
                );
                store.forget_workspace(&workspace.repo, &workspace.workspace_id)?;
                return Err(CubeError::NoAvailableWorkspace(repo));
            }

            // If the workspace had conflicted bookmarks, repair them before
            // the reset. `jj new main` would succeed with conflicts present,
            // but the conflicts would still appear in `jj status` for the
            // new worker — better to clean them up now so the workspace is
            // truly pristine.
            if !repair_bookmarks.is_empty()
                && let Err(error) = repair_conflicted_bookmarks(
                    runner,
                    database_path,
                    &workspace.workspace_path,
                    &repair_bookmarks,
                ) {
                    let _ = store.release_workspace(&lease_id, Some("lease_setup_failed"));
                    return Err(error);
                }

            // If the workspace we just claimed was reclaimed-from-expired
            // in this lease call, guard the reset: a destructive
            // `jj new <main>` against a workspace whose prior lease
            // holder is still active would silently destroy their
            // working copy. This is exactly the race Worf reported on
            // 2026-05-12 ("`@` got re-pointed at unrelated commits").
            //
            // Auto-created workspaces just came out of `jj git clone`,
            // so there is no prior worker's `@` to protect — skip the
            // guard in that case to avoid spurious refusals after the
            // reconcile-and-replace path discards a dangling row.
            let prior_expired = if was_auto_created {
                None
            } else {
                expired_by_workspace_id.get(workspace.workspace_id.as_str()).copied()
            };
            let resume_info = if let Some(pr_number) = resume_pr {
                match resume_workspace_on_pr(
                    runner,
                    database_path,
                    &workspace.workspace_path,
                    pr_number,
                    prior_expired,
                    &repo_record.main_branch,
                ) {
                    Ok(info) => Some(info),
                    Err(e) => {
                        let _ = store.release_workspace(&lease_id, Some("lease_setup_failed"));
                        return Err(e);
                    }
                }
            } else {
                if let Err(error) = reset_workspace_guarded(
                    runner,
                    database_path,
                    &workspace.workspace_path,
                    &repo_record.main_branch,
                    prior_expired,
                ) {
                    let _ = store.release_workspace(&lease_id, Some("lease_setup_failed"));
                    return Err(error);
                }
                None
            };

            let head_commit =
                current_workspace_commit(runner, database_path, &workspace.workspace_path)?;
            store.update_workspace_head_commit(&lease_id, Some(&head_commit))?;
            workspace.head_commit = Some(head_commit);

            audit!(
                database_path,
                "lease.acquired",
                repo = workspace.repo,
                workspace_id = workspace.workspace_id,
                lease_id = lease_id,
                holder = holder,
                task = task,
                head_commit = workspace.head_commit,
                resume_pr_number = resume_info.as_ref().map(|i| i.pr_number),
                resume_head_branch = resume_info.as_ref().map(|i| i.head_branch.as_str()),
            );

            // Defense-in-depth (issue #1174): keep Boss/host infra files —
            // an empty `logs/<workspace>.log`, the engine's `.boss/` scratch
            // dir — out of the worker's jj snapshot so they never get
            // committed into a PR. Done before setup runs, so a setup step
            // that drops such a file is already covered.
            ensure_boss_infra_excluded(&workspace.workspace_path, &workspace.workspace_id);

            let setup_report = run_setup_for_workspace(&store, runner, &workspace)?;

            let lease_message = format!(
                "Leased {} at {}.",
                workspace.workspace_id,
                workspace.workspace_path.display()
            );

            if let Some(failure) = setup_report.first_failure() {
                // Lease is intentionally retained: the workspace is leased
                // but its setup needs repair (`cube workspace setup`) or
                // explicit release. The error surfaces the failed step so
                // callers can decide how to recover.
                let StepStatus::Failed { error } = &failure.status else {
                    unreachable!("first_failure returned non-failure step");
                };
                return Err(CubeError::SetupStepFailed {
                    step: failure.id.clone(),
                    error: error.clone(),
                });
            }

            let message = format_lease_message(&lease_message, &setup_report);
            let mut payload = json!({
                "workspace": workspace,
                "setup": setup_report,
                "health_check": health_checks,
            });
            if let Some(ref info) = resume_info {
                payload["resume_pr"] = json!({
                    "pr_number": info.pr_number,
                    "head_branch": info.head_branch,
                });
            }
            RunResult::new(message, payload)
        }
        WorkspaceCommand::Release {
            workspace,
            lease,
            repo,
            reason,
            keep_dirty,
        } => {
            let lease = resolve_release_lease(&mut store, workspace, lease, repo)?;
            let workspace = store
                .get_workspace_by_lease(&lease)?
                .ok_or_else(|| CubeError::LeaseNotFound(lease.clone()))?;
            let _lock = RepoLock::acquire(&repo_lock_path(&workspace.repo, database_path)?)?;
            if !workspace_path_exists(&workspace) {
                eprintln!(
                    "warning: cube workspace `{}/{}` directory is missing at {}; \
                     removing the dangling registry row instead of running release reset",
                    workspace.repo,
                    workspace.workspace_id,
                    workspace.workspace_path.display(),
                );
                audit!(
                    database_path,
                    "workspace.dir_missing_reconciled",
                    repo = workspace.repo,
                    workspace_id = workspace.workspace_id,
                    workspace_path = workspace.workspace_path.display().to_string(),
                    prior_state = workspace.state.as_str(),
                    lease_id = lease,
                );
                store.forget_workspace(&workspace.repo, &workspace.workspace_id)?;
                return Err(CubeError::LeaseNotFound(lease));
            }
            if !keep_dirty {
                let repo_record = store
                    .get_repo(&workspace.repo)?
                    .ok_or_else(|| CubeError::RepoNotFound(workspace.repo.clone()))?;
                reset_workspace(
                    runner,
                    database_path,
                    &workspace.workspace_path,
                    &repo_record.main_branch,
                )?;
                // Opportunistically forget consumed boss/exec_* bookmarks.
                // The fetch above already updated main, so do_fetch = false.
                // Best-effort: log a warning but never block the release.
                match gc_workspace_bookmarks(
                    runner,
                    database_path,
                    &workspace.workspace_path,
                    false,
                    false,
                ) {
                    Ok(forgotten) if !forgotten.is_empty() => {
                        eprintln!(
                            "cube: release gc: {} consumed bookmark(s) forgotten in {}",
                            forgotten.len(),
                            workspace.workspace_id,
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!(
                            "warning: bookmark gc on release of {} failed: {e}",
                            workspace.workspace_id,
                        );
                    }
                }
            }
            let released = store
                .release_workspace(&lease, reason.as_deref())?
                .ok_or_else(|| CubeError::LeaseNotFound(lease.clone()))?;

            audit!(
                database_path,
                "lease.released",
                repo = released.repo,
                workspace_id = released.workspace_id,
                lease_id = lease,
                reason = reason,
                keep_dirty = keep_dirty,
            );

            let message = if keep_dirty {
                format!("Released {} (kept dirty).", released.workspace_id)
            } else {
                format!("Released {}.", released.workspace_id)
            };
            RunResult::new(
                message,
                json!({
                    "workspace": released,
                }),
            )
        }
        WorkspaceCommand::Heartbeat { lease, ttl_seconds } => {
            let now = current_epoch_s()?;
            let ttl = ttl_seconds
                .map(|s| s as i64)
                .unwrap_or(DEFAULT_LEASE_TTL_SECS);
            let new_expires_at = now + ttl;
            let updated = store
                .heartbeat_lease(&lease, Some(new_expires_at))?
                .ok_or_else(|| CubeError::LeaseNotFound(lease.clone()))?;
            RunResult::new(
                format!(
                    "Heartbeat lease {}; new expiry {} (in {}s).",
                    lease, new_expires_at, ttl
                ),
                json!({
                    "workspace": updated,
                }),
            )
        }
        WorkspaceCommand::ForceRelease {
            workspace,
            lease,
            repo,
            reason,
        } => {
            let lease = resolve_release_lease(&mut store, workspace, lease, repo)?;
            // Repo-scoped lock so a concurrent normal release can't race.
            let workspace_record = store
                .get_workspace_by_lease(&lease)?
                .ok_or_else(|| CubeError::LeaseNotFound(lease.clone()))?;
            let _lock = RepoLock::acquire(&repo_lock_path(&workspace_record.repo, database_path)?)?;
            let reason = reason.unwrap_or_else(|| "force-released".to_string());
            let released = store
                .force_release_lease(&lease, Some(&reason))?
                .ok_or_else(|| CubeError::LeaseNotFound(lease.clone()))?;

            audit!(
                database_path,
                "lease.force_released",
                repo = released.repo,
                workspace_id = released.workspace_id,
                lease_id = lease,
                reason = reason,
            );

            RunResult::new(
                format!(
                    "Force-released {} (workspace not reset).",
                    released.workspace_id
                ),
                json!({
                    "workspace": released,
                }),
            )
        }
        WorkspaceCommand::Status { workspace } => {
            let path = PathBuf::from(&workspace);
            let record = find_workspace_record(&mut store, &path)?
                .ok_or_else(|| CubeError::WorkspaceNotFound(workspace.clone()))?;
            let jj_status = run_jj(
                runner,
                database_path,
                &RealCommandRunner::invocation(&path, "jj", &["status"]),
            )?;

            RunResult::new(
                human_workspace_detail(&record, &jj_status),
                json!({
                    "workspace": record,
                    "jj_status": jj_status,
                }),
            )
        }
        WorkspaceCommand::Setup { workspace } => {
            let path = PathBuf::from(&workspace);
            let record = find_workspace_record(&mut store, &path)?
                .ok_or_else(|| CubeError::WorkspaceNotFound(workspace.clone()))?;
            let report = run_setup_for_workspace(&store, runner, &record)?;
            let payload = json!({
                "workspace": record,
                "setup": report,
            });
            if let Some(failure) = report.first_failure() {
                let StepStatus::Failed { error } = &failure.status else {
                    unreachable!("first_failure returned non-failure step");
                };
                return Err(CubeError::SetupStepFailed {
                    step: failure.id.clone(),
                    error: error.clone(),
                });
            }
            let message = format_setup_message(&record.workspace_id, &report);
            RunResult::new(message, payload)
        }
        WorkspaceCommand::List {
            repo,
            state,
            holder,
        } => {
            let parsed_effective_state = match state.as_deref() {
                Some(raw) => Some(EffectiveState::from_str(raw).ok_or_else(|| {
                    CubeError::InvalidArgument(format!(
                        "invalid --state `{raw}`; expected `free`, `free-dirty`, `free-conflicted`, or `leased`"
                    ))
                })?),
                None => None,
            };
            // Reconcile rows whose on-disk directory has been wiped before
            // we materialize the listing — otherwise `list` would surface
            // a row that the next `lease` is going to fail on. Scope the
            // reconcile to the same repo filter the user asked for.
            let reconciled = reconcile_missing_workspaces(
                &mut store,
                database_path,
                repo.as_deref(),
                current_epoch_s()?,
            )?;
            let filter = WorkspaceListFilter {
                repo: repo.as_deref(),
                effective_state: parsed_effective_state,
                holder_pattern: holder.as_deref(),
                ..Default::default()
            };
            let records = store.list_workspaces_filtered(&filter)?;
            let message = format_workspace_list(&records);
            RunResult::new(
                message,
                json!({
                    "workspaces": records,
                    "reconciled": reconciled,
                }),
            )
        }
        WorkspaceCommand::Remove {
            workspace,
            repo,
            force,
            expunge,
        } => {
            let matches = store.list_workspaces_filtered(&WorkspaceListFilter {
                repo: repo.as_deref(),
                workspace_id: Some(&workspace),
                ..Default::default()
            })?;
            let record = match matches.as_slice() {
                [] => return Err(CubeError::WorkspaceNotFound(workspace)),
                [single] => single.clone(),
                many => {
                    let repos = many
                        .iter()
                        .map(|r| r.repo.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(CubeError::InvalidArgument(format!(
                        "workspace id `{workspace}` matches multiple repos ({repos}); disambiguate with --repo"
                    )));
                }
            };

            let _lock = RepoLock::acquire(&repo_lock_path(&record.repo, database_path)?)?;

            if record.state == WorkspaceState::Leased && !force {
                return Err(CubeError::InvalidArgument(format!(
                    "workspace `{}/{}` is currently leased; force-release it first or pass --force",
                    record.repo, record.workspace_id
                )));
            }

            store.forget_workspace(&record.repo, &record.workspace_id)?;

            if expunge {
                match fs::remove_dir_all(&record.workspace_path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => {
                        return Err(CubeError::WorkspaceDirRemove {
                            path: record.workspace_path.clone(),
                            source: err,
                        })
                    }
                }
            }

            if let Err(e) = cleanup_workspace_logs(&record.workspace_id) {
                eprintln!(
                    "warning: failed to clean up workspace logs for {}: {e}",
                    record.workspace_id
                );
            }

            audit!(
                database_path,
                "workspace.removed",
                repo = record.repo,
                workspace_id = record.workspace_id,
                prior_state = record.state.as_str(),
                lease_id = record.lease_id,
                holder = record.holder,
                forced = force,
                expunged = expunge,
            );

            let message = if expunge {
                format!(
                    "Removed {}/{} from the registry and deleted workspace directory at {}.",
                    record.repo,
                    record.workspace_id,
                    record.workspace_path.display(),
                )
            } else {
                format!(
                    "Removed {}/{} from the registry (workspace directory left intact at {}).",
                    record.repo,
                    record.workspace_id,
                    record.workspace_path.display(),
                )
            };
            RunResult::new(
                message,
                json!({
                    "workspace": record,
                    "forced": force,
                    "expunged": expunge,
                }),
            )
        }
        WorkspaceCommand::Gc { workspace, dry_run } => {
            let records = store.list_workspaces_filtered(&WorkspaceListFilter {
                workspace_id: workspace.as_deref(),
                ..Default::default()
            })?;

            #[derive(serde::Serialize)]
            struct WorkspaceGcResult {
                workspace_id: String,
                bookmarks_forgotten: Vec<String>,
                skipped: bool,
                skipped_reason: Option<String>,
                error: Option<String>,
            }

            let mut results: Vec<WorkspaceGcResult> = Vec::new();
            for record in &records {
                if record.state == WorkspaceState::Leased {
                    results.push(WorkspaceGcResult {
                        workspace_id: record.workspace_id.clone(),
                        bookmarks_forgotten: vec![],
                        skipped: true,
                        skipped_reason: Some("leased".to_string()),
                        error: None,
                    });
                    continue;
                }
                if !workspace_path_exists(record) {
                    results.push(WorkspaceGcResult {
                        workspace_id: record.workspace_id.clone(),
                        bookmarks_forgotten: vec![],
                        skipped: true,
                        skipped_reason: Some("directory_missing".to_string()),
                        error: None,
                    });
                    continue;
                }
                match gc_workspace_bookmarks(
                    runner,
                    database_path,
                    &record.workspace_path,
                    true,
                    dry_run,
                ) {
                    Ok(bookmarks) => {
                        results.push(WorkspaceGcResult {
                            workspace_id: record.workspace_id.clone(),
                            bookmarks_forgotten: bookmarks,
                            skipped: false,
                            skipped_reason: None,
                            error: None,
                        });
                    }
                    Err(e) => {
                        results.push(WorkspaceGcResult {
                            workspace_id: record.workspace_id.clone(),
                            bookmarks_forgotten: vec![],
                            skipped: false,
                            skipped_reason: None,
                            error: Some(e.to_string()),
                        });
                    }
                }
            }

            let total_forgotten: usize =
                results.iter().map(|r| r.bookmarks_forgotten.len()).sum();
            let message = if dry_run {
                format!(
                    "{} workspace(s): {} bookmark(s) would be forgotten (dry-run).",
                    results.len(),
                    total_forgotten
                )
            } else {
                format!(
                    "{} workspace(s): {} bookmark(s) forgotten.",
                    results.len(),
                    total_forgotten
                )
            };
            RunResult::new(message, json!({ "results": results }))
        }
    }
}

fn run_change(
    command: ChangeCommand,
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
) -> Result<RunResult> {
    let mut store = if let Some(path) = database_path {
        Store::open_at(path)?
    } else {
        Store::open_default()?
    };

    match command {
        ChangeCommand::Create(args) => {
            if args.workspace.is_some() && args.parent.is_some() {
                return Err(CubeError::InvalidArgument(
                    "change create accepts either --workspace or --parent, not both".to_string(),
                ));
            }
            if args.workspace.is_none() && args.parent.is_none() {
                return Err(CubeError::InvalidArgument(
                    "change create requires --workspace or --parent".to_string(),
                ));
            }

            let (repo, workspace_path, parent_change_id) = if let Some(workspace) = args.workspace {
                let workspace_path = PathBuf::from(&workspace);
                let record = find_workspace_record(&mut store, &workspace_path)?
                    .ok_or_else(|| CubeError::WorkspaceNotFound(workspace.clone()))?;
                (record.repo, workspace_path, None)
            } else if let Some(parent_change_id) = args.parent {
                let parent = store
                    .get_change(&parent_change_id)?
                    .ok_or_else(|| CubeError::ChangeNotFound(parent_change_id.clone()))?;
                (parent.repo, parent.workspace_path, Some(parent.change_id))
            } else {
                unreachable!("validated create arguments");
            };

            if let Some(parent_change_id) = parent_change_id.as_deref() {
                let parent = store
                    .get_change(parent_change_id)?
                    .ok_or_else(|| CubeError::ChangeNotFound(parent_change_id.to_string()))?;
                run_jj(
                    runner,
                    database_path,
                    &CommandInvocation {
                        cwd: workspace_path.clone(),
                        program: "jj".to_string(),
                        args: vec![
                            "new".to_string(),
                            parent.jj_change_id,
                            "-m".to_string(),
                            args.title.clone(),
                        ],
                    },
                )?;
            } else {
                run_jj(
                    runner,
                    database_path,
                    &CommandInvocation {
                        cwd: workspace_path.clone(),
                        program: "jj".to_string(),
                        args: vec!["describe".to_string(), "-m".to_string(), args.title.clone()],
                    },
                )?;
            }

            let identity = current_change_identity(runner, database_path, &workspace_path)?;
            let change_id = format!("chg_{}", Uuid::new_v4().simple());
            let record = store.insert_change(&ChangeRecord {
                change_id,
                repo,
                workspace_path,
                parent_change_id,
                title: args.title,
                jj_change_id: identity.jj_change_id,
                head_commit: identity.head_commit,
                created_at_epoch_s: current_epoch_s()?,
            })?;

            RunResult::new(
                format!("Created change `{}`.", record.change_id),
                json!({
                    "change": record,
                }),
            )
        }
        ChangeCommand::Checkout { change } => Err(CubeError::NotImplemented(format!(
            "change command `checkout` is not implemented yet for `{change}`"
        ))),
        ChangeCommand::Info { change } => {
            let record = store
                .get_change(&change)?
                .ok_or_else(|| CubeError::ChangeNotFound(change.clone()))?;
            RunResult::new(
                human_change_detail(&record),
                json!({
                    "change": record,
                }),
            )
        }
    }
}

fn run_stack(command: StackCommand) -> Result<RunResult> {
    Err(CubeError::NotImplemented(format!(
        "stack command `{}` is not implemented yet",
        stack_command_name(&command)
    )))
}

fn run_pr(command: PrCommand, runner: &dyn CommandRunner) -> Result<RunResult> {
    match command {
        PrCommand::Ensure(args) => ensure_pr(args, runner),
        PrCommand::Push(args) => pr_push(args, runner),
        _ => Err(CubeError::NotImplemented(format!(
            "pr command `{}` is not implemented yet",
            pr_command_name(&command)
        ))),
    }
}

/// Returns true if `path` is a reference to stdin (`/dev/stdin`, `-`, `/dev/fd/0`).
fn is_stdin_path(path: &str) -> bool {
    matches!(path, "/dev/stdin" | "-" | "/dev/fd/0")
}

/// Resolve `--body-file <path>` to a concrete filesystem path, materialising
/// stdin and pipe/FIFO sources eagerly.
///
/// Returns `(resolved_path_string, Option<temp_file_path>)`.  When a temp
/// file was created the caller is responsible for deleting it (the `PathBuf`
/// is returned so the caller can `fs::remove_file` it after the subprocess
/// finishes).
///
/// Fails loudly if the body source is empty — an empty description is almost
/// certainly a bug, not intentional.
fn resolve_body_file(path: &str) -> Result<(String, Option<PathBuf>)> {
    use std::io::Read;

    // Decide whether we need to slurp the content into memory.
    let is_stdin_like = is_stdin_path(path);

    #[cfg(unix)]
    let is_pipe_or_special = if is_stdin_like {
        true
    } else {
        use std::os::unix::fs::FileTypeExt;
        match std::fs::metadata(path) {
            Ok(meta) => meta.file_type().is_fifo() || meta.file_type().is_char_device(),
            Err(_) => false,
        }
    };

    #[cfg(not(unix))]
    let is_pipe_or_special = is_stdin_like;

    if is_pipe_or_special {
        // Slurp eagerly before any subprocess can race on the fd.
        let mut content = String::new();
        if is_stdin_like {
            std::io::stdin()
                .read_to_string(&mut content)
                .map_err(CubeError::Io)?;
        } else {
            std::fs::File::open(path)
                .and_then(|mut f| f.read_to_string(&mut content))
                .map_err(CubeError::Io)?;
        }

        if content.trim().is_empty() {
            return Err(CubeError::InvalidArgument(format!(
                "--body-file `{path}` produced empty content; \
                 refusing to create a PR with no description"
            )));
        }

        // Write to a uniquely-named temp file so gh pr create can open it as
        // a regular file (no race, no /dev/stdin weirdness in the subprocess).
        let tmp_path = std::env::temp_dir()
            .join(format!("cube-pr-body-{}.md", Uuid::new_v4()));
        std::fs::write(&tmp_path, content.as_bytes()).map_err(CubeError::Io)?;
        let tmp_path_str = tmp_path.display().to_string();
        Ok((tmp_path_str, Some(tmp_path)))
    } else {
        // Regular file path — validate it exists and is non-empty.
        let meta = std::fs::metadata(path).map_err(|e| {
            CubeError::InvalidArgument(format!("--body-file `{path}`: {e}"))
        })?;
        if meta.len() == 0 {
            return Err(CubeError::InvalidArgument(format!(
                "--body-file `{path}` is empty; \
                 refusing to create a PR with no description"
            )));
        }
        Ok((path.to_string(), None))
    }
}

/// Create or reuse a GitHub PR for the current jj bookmark.
///
/// Pushes the branch via `jj git push` and then uses `gh pr create -R
/// <owner/repo>` — no `GIT_DIR` guess needed, works from both primary
/// and secondary cube workspaces.
fn ensure_pr(args: PrEnsureArgs, runner: &dyn CommandRunner) -> Result<RunResult> {
    let cwd = std::env::current_dir().map_err(CubeError::Io)?;

    // Resolve owner/repo from jj remote list.
    let remote_output = runner
        .run(&RealCommandRunner::invocation(
            &cwd,
            "jj",
            &["git", "remote", "list"],
        ))
        .map_err(|e| {
            CubeError::InvalidArgument(format!(
                "failed to list jj remotes (is this a jj workspace?): {e}"
            ))
        })?;
    // Resolve BOTH the remote *name* and the owner/repo slug. The name
    // matters: in a cube workspace `origin` is a local on-disk mirror and
    // the real GitHub upstream is a differently-named remote (commonly
    // `github`). `jj git push` without an explicit `--remote` would target
    // jj's default remote — which may be that local mirror — silently
    // updating a ref that never reaches GitHub. We push to the github.com
    // remote by name to avoid that trap.
    let (github_remote, owner_repo) = parse_github_remote(&remote_output).ok_or_else(|| {
        CubeError::InvalidArgument(format!(
            "could not detect a github.com remote from `jj git remote list` output:\n{remote_output}"
        ))
    })?;

    // Determine branch name.
    let branch = match args.branch {
        Some(b) => b,
        None => detect_jj_bookmark(runner, &cwd)?,
    };

    // Refuse to push a `pr/<n>` bookmark — those are local-only cube
    // bookkeeping and must never reach a remote.
    pr_bookmark::assert_not_pr_bookmark(&branch)
        .map_err(CubeError::InvalidArgument)?;

    // Push the branch to the GitHub remote by name (--allow-new is
    // idempotent: fine when the remote bookmark already exists).
    runner
        .run(&RealCommandRunner::invocation(
            &cwd,
            "jj",
            &[
                "git", "push", "-b", &branch, "--remote", &github_remote, "--allow-new",
            ],
        ))
        .map_err(|e| {
            CubeError::InvalidArgument(format!("failed to push branch `{branch}`: {e}"))
        })?;

    // Verify the push actually reached GitHub. Confirming against the same
    // remote we pushed to (e.g. `git ls-remote origin`) is circular — if
    // that remote is a local mirror it reports success while GitHub stays
    // stale. Instead we read GitHub's own truth (the branch head sha) and
    // assert it matches the local commit, failing loudly on mismatch.
    verify_push_reached_github(runner, &cwd, &owner_repo, &branch)?;

    // Check for existing open PRs. Using --state open is explicit: gh pr list
    // defaults to open-only, but being explicit guards against any default drift.
    let list_json = runner
        .run(&RealCommandRunner::invocation(
            &cwd,
            "gh",
            &[
                "pr",
                "list",
                "-R",
                &owner_repo,
                "--head",
                &branch,
                "--state",
                "open",
                "--json",
                "url",
            ],
        ))
        .map_err(|e| {
            CubeError::InvalidArgument(format!("failed to check for existing PR: {e}"))
        })?;

    let prs = serde_json::from_str::<Vec<serde_json::Value>>(&list_json).unwrap_or_default();

    if prs.len() > 1 {
        return Err(CubeError::InvalidArgument(format!(
            "found {} open PRs for branch `{branch}` — expected at most 1. \
             Close duplicate PRs before retrying.",
            prs.len()
        )));
    }

    if let Some(url) = prs
        .first()
        .and_then(|pr| pr.get("url"))
        .and_then(|v| v.as_str())
    {
        let url = url.to_string();
        let number = pr_number_from_url(&url);
        let pr_bookmark_name = set_pr_bookmark(runner, &cwd, number, &branch)?;
        return RunResult::new(
            url.clone(),
            json!({"action": "exists", "url": url, "number": number, "pr_bookmark": pr_bookmark_name}),
        );
    }

    // No existing PR — create one.
    let mut create_args: Vec<&str> = vec![
        "pr", "create", "-R", &owner_repo, "--head", &branch, "--base", "main",
    ];
    let title_ref;
    let body_ref;
    // Materialised path for --body-file (may differ from the original when
    // the source was stdin / a pipe).  Keep `tmp_body_path` alive until after
    // the gh subprocess exits so the temp file isn't deleted underneath it.
    let body_file_resolved;
    let tmp_body_path: Option<PathBuf>;
    if let Some(ref t) = args.title {
        title_ref = t.as_str();
        create_args.push("--title");
        create_args.push(title_ref);
    }
    if let Some(ref b) = args.body {
        body_ref = b.as_str();
        create_args.push("--body");
        create_args.push(body_ref);
    }
    if let Some(ref f) = args.body_file {
        let (resolved, tmp) = resolve_body_file(f)?;
        body_file_resolved = resolved;
        tmp_body_path = tmp;
        create_args.push("--body-file");
        create_args.push(&body_file_resolved);
    } else {
        tmp_body_path = None;
    }
    if args.draft {
        create_args.push("--draft");
    }

    let create_output = runner
        .run(&RealCommandRunner::invocation(&cwd, "gh", &create_args))
        .map_err(|e| CubeError::InvalidArgument(format!("failed to create PR: {e}")))?;

    // Clean up any temp file we created to materialise a piped body source.
    if let Some(ref p) = tmp_body_path {
        let _ = std::fs::remove_file(p);
    }

    let url = create_output.trim().to_string();
    if url.is_empty() {
        return Err(CubeError::InvalidArgument(
            "gh pr create produced no output — PR may not have been created".to_string(),
        ));
    }
    let number = pr_number_from_url(&url);
    let pr_bookmark_name = set_pr_bookmark(runner, &cwd, number, &branch)?;
    RunResult::new(
        url.clone(),
        json!({"action": "created", "url": url, "number": number, "pr_bookmark": pr_bookmark_name}),
    )
}

/// Sets the local `pr/<n>` bookmark on the given branch.
///
/// Returns the bookmark name if the number was resolved, or `None` if the PR
/// URL didn't contain a parseable number (so callers can include it in JSON).
fn set_pr_bookmark(
    runner: &dyn CommandRunner,
    cwd: &Path,
    number: Option<u64>,
    branch: &str,
) -> Result<Option<String>> {
    let Some(n) = number else {
        return Ok(None);
    };
    let bookmark_name = pr_bookmark::pr_bookmark_name(n);
    runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "jj",
            &["bookmark", "set", &bookmark_name, "-r", branch],
        ))
        .map_err(|e| {
            CubeError::InvalidArgument(format!(
                "failed to set local bookmark `{bookmark_name}`: {e}"
            ))
        })?;
    Ok(Some(bookmark_name))
}

/// Verify that a just-pushed branch actually reached GitHub.
///
/// Reads the branch head sha from GitHub's API (the authoritative source)
/// and compares it to the local commit the bookmark points at. This closes
/// the "false confirmation" hole where a push lands on a local mirror
/// remote and a same-remote check (`git ls-remote <that remote>`) reports
/// success even though GitHub — and therefore any open PR — never advanced.
fn verify_push_reached_github(
    runner: &dyn CommandRunner,
    cwd: &Path,
    owner_repo: &str,
    branch: &str,
) -> Result<()> {
    let local_sha = runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "jj",
            &["log", "-r", branch, "--no-graph", "-T", "commit_id"],
        ))
        .map_err(|e| {
            CubeError::InvalidArgument(format!(
                "could not resolve local commit for `{branch}` to verify the push: {e}"
            ))
        })?;
    let local_sha = local_sha.trim();

    let api_path = format!("repos/{owner_repo}/branches/{branch}");
    let remote_sha = runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "gh",
            &["api", &api_path, "--jq", ".commit.sha"],
        ))
        .map_err(|e| {
            CubeError::InvalidArgument(format!(
                "push verification failed: could not read branch `{branch}` from GitHub \
                 ({owner_repo}). The push may have gone to a local mirror remote instead of \
                 GitHub — in cube workspaces the real upstream is the github.com remote, not \
                 necessarily `origin`. Underlying error: {e}"
            ))
        })?;
    let remote_sha = remote_sha.trim();

    if local_sha != remote_sha {
        return Err(CubeError::InvalidArgument(format!(
            "push verification failed: local `{branch}` is at {local_sha} but GitHub \
             ({owner_repo}) has it at {remote_sha}. The push did not reach GitHub — it likely \
             landed on a local mirror remote. Re-push to the github.com remote, then re-verify \
             against `gh api repos/{owner_repo}/branches/{branch} --jq .commit.sha`."
        )));
    }
    Ok(())
}

/// Extract the PR number from a GitHub pull request URL.
///
/// Returns `None` if the URL does not end with a numeric segment.
fn pr_number_from_url(url: &str) -> Option<u64> {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .and_then(|s| s.parse().ok())
}

/// Detect the first bookmark name on the current jj commit (`@`).
fn detect_jj_bookmark(runner: &dyn CommandRunner, cwd: &Path) -> Result<String> {
    let output = runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "jj",
            &[
                "log",
                "-r",
                "@",
                "--no-graph",
                "-T",
                r#"bookmarks.map(|b| b.name()).join("\n")"#,
            ],
        ))
        .map_err(|e| {
            CubeError::InvalidArgument(format!(
                "failed to detect current jj bookmark: {e}"
            ))
        })?;

    output
        .lines()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .ok_or_else(|| {
            CubeError::InvalidArgument(
                "no bookmark on current jj commit — run `jj bookmark create <name> -r @` first"
                    .to_string(),
            )
        })
        .map(str::to_string)
}

/// Advance an existing PR by pushing the current commit (`@`) to its head branch.
///
/// Implements the `cube pr push` subcommand. Advances both the remote head
/// branch and the local `pr/<n>` bookmark to `@` (fast-forward only by
/// default) and verifies the push reached GitHub.
fn pr_push(args: PrPushArgs, runner: &dyn CommandRunner) -> Result<RunResult> {
    let cwd = std::env::current_dir().map_err(CubeError::Io)?;

    // Resolve owner/repo and the github remote name.
    let remote_output = runner
        .run(&RealCommandRunner::invocation(
            &cwd,
            "jj",
            &["git", "remote", "list"],
        ))
        .map_err(|e| {
            CubeError::InvalidArgument(format!(
                "failed to list jj remotes (is this a jj workspace?): {e}"
            ))
        })?;
    let (github_remote, owner_repo) = parse_github_remote(&remote_output).ok_or_else(|| {
        CubeError::InvalidArgument(format!(
            "could not detect a github.com remote from `jj git remote list` output:\n{remote_output}"
        ))
    })?;

    // Resolve (pr_number, head_branch) from args or by inference.
    let (pr_number, head_branch) =
        resolve_pr_push_target(&args, runner, &cwd, &github_remote, &owner_repo)?;

    // Guard: the head branch must not be a reserved pr/* bookmark.
    pr_bookmark::assert_not_pr_bookmark(&head_branch)
        .map_err(CubeError::InvalidArgument)?;

    let pr_bm = pr_bookmark::pr_bookmark_name(pr_number);

    // Check that the PR is still open — refuse to push onto a merged/closed PR.
    check_pr_open(runner, &cwd, &owner_repo, pr_number)?;

    // Trigger jj's working-copy snapshot and check if @ is empty.
    let empty_out = runner
        .run(&RealCommandRunner::invocation(
            &cwd,
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "empty"],
        ))
        .map_err(|e| {
            CubeError::InvalidArgument(format!("failed to inspect working copy: {e}"))
        })?;
    let at_is_empty = empty_out.trim() == "true";

    if at_is_empty {
        // @ is empty: this is either a no-op (already pushed) or a "nothing to land" error.
        // Check whether the pr/<n> bookmark and GitHub are already in sync.
        let github_sha = fetch_github_sha(runner, &cwd, &owner_repo, &head_branch)?;
        let pr_bm_sha_result = runner.run(&RealCommandRunner::invocation(
            &cwd,
            "jj",
            &["log", "-r", &pr_bm, "--no-graph", "-T", "commit_id"],
        ));
        match pr_bm_sha_result {
            Ok(sha) if sha.trim() == github_sha.trim() => {
                // Bookmarks and GitHub are already in sync — idempotent no-op.
                let pr_url = format!("https://github.com/{owner_repo}/pull/{pr_number}");
                return RunResult::new(
                    pr_url.clone(),
                    json!({"action": "noop", "url": pr_url, "number": pr_number}),
                );
            }
            _ => {
                return Err(CubeError::InvalidArgument(
                    "@ is empty — nothing to land; create a commit before running `cube pr push`"
                        .to_string(),
                ));
            }
        }
    }

    // For force-with-lease: skip the descendant check (lease verification is the safety instead).
    // For normal push: @ must be a descendant of pr/<n> (fast-forward enforcement).
    if args.force_with_lease {
        // Lease verification: jj's last-fetched remote state must match GitHub.
        let remote_ref = format!("{head_branch}@{github_remote}");
        let fetched_sha = runner
            .run(&RealCommandRunner::invocation(
                &cwd,
                "jj",
                &["log", "-r", &remote_ref, "--no-graph", "-T", "commit_id"],
            ))
            .map_err(|e| {
                CubeError::InvalidArgument(format!(
                    "failed to read last-fetched state of `{remote_ref}`: {e}; \
                     run `jj git fetch` before `cube pr push --force-with-lease`"
                ))
            })?;
        let fetched_sha = fetched_sha.trim();
        let github_sha = fetch_github_sha(runner, &cwd, &owner_repo, &head_branch)?;
        let github_sha = github_sha.trim();
        if fetched_sha != github_sha {
            return Err(CubeError::InvalidArgument(format!(
                "force-with-lease refused: `{head_branch}` on GitHub ({github_sha}) has advanced \
                 beyond the last-fetched state ({fetched_sha}). Another workspace pushed \
                 concurrently. Run `jj git fetch` and decide whether to rebase before \
                 force-pushing."
            )));
        }

        // Advance both bookmarks to @.
        advance_pr_bookmarks(runner, &cwd, &head_branch, &pr_bm)?;

        // Force push via git (jj git push has no --force-with-lease flag).
        runner
            .run(&RealCommandRunner::invocation(
                &cwd,
                "git",
                &["push", "--force-with-lease", &github_remote, &head_branch],
            ))
            .map_err(|e| {
                CubeError::InvalidArgument(format!(
                    "force-with-lease push of `{head_branch}` failed: {e}"
                ))
            })?;
    } else {
        // Normal fast-forward push: @ must be a descendant of pr/<n>.
        let ancestor_rev = format!("{pr_bm} & ancestors(@)");
        let ancestor_out = runner
            .run(&RealCommandRunner::invocation(
                &cwd,
                "jj",
                &["log", "-r", &ancestor_rev, "--no-graph", "-T", "commit_id"],
            ))
            .map_err(|e| {
                CubeError::InvalidArgument(format!(
                    "failed to check ancestry of `{pr_bm}`: {e}"
                ))
            })?;
        if ancestor_out.trim().is_empty() {
            return Err(CubeError::InvalidArgument(format!(
                "@ is not a descendant of `{pr_bm}` — refusing to push (this would not be a \
                 fast-forward). Use `--force-with-lease` for rewrite scenarios, or run \
                 `cube workspace lease --resume_pr {pr_number}` to rebuild on the current head."
            )));
        }

        // Advance both bookmarks to @.
        advance_pr_bookmarks(runner, &cwd, &head_branch, &pr_bm)?;

        // Push the head branch (no --allow-new: the branch already exists remotely).
        runner
            .run(&RealCommandRunner::invocation(
                &cwd,
                "jj",
                &[
                    "git",
                    "push",
                    "-b",
                    &head_branch,
                    "--remote",
                    &github_remote,
                ],
            ))
            .map_err(|e| {
                CubeError::InvalidArgument(format!("failed to push `{head_branch}`: {e}"))
            })?;
    }

    // Verify the push reached GitHub.
    verify_push_reached_github(runner, &cwd, &owner_repo, &head_branch)?;

    let pr_url = format!("https://github.com/{owner_repo}/pull/{pr_number}");
    RunResult::new(
        pr_url.clone(),
        json!({"action": "pushed", "url": pr_url, "number": pr_number}),
    )
}

/// Resolve (pr_number, head_branch) for `cube pr push` from args and/or jj ancestry.
fn resolve_pr_push_target(
    args: &PrPushArgs,
    runner: &dyn CommandRunner,
    cwd: &Path,
    _github_remote: &str,
    owner_repo: &str,
) -> Result<(u64, String)> {
    match (args.pr, args.branch.as_deref()) {
        (Some(n), Some(b)) => Ok((n, b.to_string())),

        (Some(n), None) => {
            // Have PR number; find head branch from the pr/<n> bookmark's co-located bookmarks.
            let pr_bm = pr_bookmark::pr_bookmark_name(n);
            let bm_out = runner
                .run(&RealCommandRunner::invocation(
                    cwd,
                    "jj",
                    &[
                        "log",
                        "-r",
                        &pr_bm,
                        "--no-graph",
                        "-T",
                        r#"bookmarks.map(|b| b.name()).join("\n")"#,
                    ],
                ))
                .map_err(|e| {
                    CubeError::InvalidArgument(format!(
                        "could not find `{pr_bm}` bookmark locally: {e}; \
                         run `cube workspace lease --resume_pr {n}` first or pass --branch"
                    ))
                })?;
            let head_branch = bm_out
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty() && !pr_bookmark::is_pr_bookmark(s))
                .next()
                .ok_or_else(|| {
                    CubeError::InvalidArgument(format!(
                        "no head branch found co-located with `{pr_bm}`; pass --branch explicitly"
                    ))
                })?
                .to_string();
            Ok((n, head_branch))
        }

        (None, Some(b)) => {
            // Have branch; find PR number from GitHub.
            let list_json = runner
                .run(&RealCommandRunner::invocation(
                    cwd,
                    "gh",
                    &[
                        "pr",
                        "list",
                        "-R",
                        owner_repo,
                        "--head",
                        b,
                        "--state",
                        "open",
                        "--json",
                        "number",
                    ],
                ))
                .map_err(|e| {
                    CubeError::InvalidArgument(format!(
                        "failed to look up open PR for branch `{b}`: {e}"
                    ))
                })?;
            let prs: Vec<serde_json::Value> =
                serde_json::from_str(&list_json).map_err(|e| {
                    CubeError::InvalidArgument(format!(
                        "unexpected response from `gh pr list` for branch `{b}`: {e}"
                    ))
                })?;
            let number = prs
                .first()
                .and_then(|pr| pr["number"].as_u64())
                .ok_or_else(|| {
                    CubeError::InvalidArgument(format!(
                        "no open PR found for branch `{b}`; create a PR with `cube pr ensure` first"
                    ))
                })?;
            Ok((number, b.to_string()))
        }

        (None, None) => {
            // Infer from @'s ancestry: find nearest commit with a pr/* bookmark.
            let infer_out = runner
                .run(&RealCommandRunner::invocation(
                    cwd,
                    "jj",
                    &[
                        "log",
                        "-r",
                        r#"latest(ancestors(@) & bookmarks(glob:"pr/*"))"#,
                        "--no-graph",
                        "-T",
                        r#"bookmarks.map(|b| b.name()).join("\n")"#,
                    ],
                ))
                .map_err(|e| {
                    CubeError::InvalidArgument(format!("failed to infer PR from ancestry: {e}"))
                })?;

            if infer_out.trim().is_empty() {
                return Err(CubeError::InvalidArgument(
                    "could not infer PR from `@`'s ancestry — no `pr/<n>` bookmark found. \
                     Pass `--pr <n>` or `--branch <name>` explicitly, or run \
                     `cube workspace lease --resume_pr <n>` to position the workspace first."
                        .to_string(),
                ));
            }

            let mut pr_number: Option<u64> = None;
            let mut head_branch: Option<String> = None;
            for name in infer_out.lines().map(str::trim).filter(|s| !s.is_empty()) {
                if pr_bookmark::is_pr_bookmark(name) {
                    if let Some(n_str) = name.strip_prefix("pr/") {
                        if let Ok(n) = n_str.parse::<u64>() {
                            pr_number = Some(n);
                        }
                    }
                } else {
                    head_branch = Some(name.to_string());
                }
            }

            match (pr_number, head_branch) {
                (Some(n), Some(b)) => Ok((n, b)),
                (Some(n), None) => Err(CubeError::InvalidArgument(format!(
                    "found `pr/{n}` in ancestry but no co-located head branch; \
                     pass --branch explicitly"
                ))),
                _ => Err(CubeError::InvalidArgument(
                    "failed to infer PR and branch from ancestry; \
                     pass --pr and --branch explicitly"
                        .to_string(),
                )),
            }
        }
    }
}

/// Verify the PR identified by `pr_number` is open on GitHub; error if merged/closed.
fn check_pr_open(
    runner: &dyn CommandRunner,
    cwd: &Path,
    owner_repo: &str,
    pr_number: u64,
) -> Result<()> {
    let state_json = runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "gh",
            &[
                "pr",
                "view",
                &pr_number.to_string(),
                "-R",
                owner_repo,
                "--json",
                "state",
            ],
        ))
        .map_err(|e| {
            CubeError::InvalidArgument(format!(
                "failed to check state of PR #{pr_number}: {e}"
            ))
        })?;
    let state: serde_json::Value = serde_json::from_str(&state_json).map_err(|e| {
        CubeError::InvalidArgument(format!(
            "unexpected response from `gh pr view {pr_number}`: {e}"
        ))
    })?;
    let state_str = state["state"].as_str().unwrap_or("UNKNOWN");
    if state_str != "OPEN" {
        return Err(CubeError::InvalidArgument(format!(
            "PR #{pr_number} is {state_str} — refusing to push onto a non-open PR. \
             Only OPEN pull requests can be advanced with `cube pr push`."
        )));
    }
    Ok(())
}

/// Fetch the current head SHA of `branch` from GitHub (authoritative source).
fn fetch_github_sha(
    runner: &dyn CommandRunner,
    cwd: &Path,
    owner_repo: &str,
    branch: &str,
) -> Result<String> {
    let api_path = format!("repos/{owner_repo}/branches/{branch}");
    runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "gh",
            &["api", &api_path, "--jq", ".commit.sha"],
        ))
        .map_err(|e| {
            CubeError::InvalidArgument(format!(
                "failed to fetch GitHub head sha for `{branch}`: {e}"
            ))
        })
}

/// Advance `head_branch` and `pr_bm` bookmarks to `@`.
fn advance_pr_bookmarks(
    runner: &dyn CommandRunner,
    cwd: &Path,
    head_branch: &str,
    pr_bm: &str,
) -> Result<()> {
    runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "jj",
            &["bookmark", "set", head_branch, "-r", "@"],
        ))
        .map_err(|e| {
            CubeError::InvalidArgument(format!(
                "failed to advance `{head_branch}` bookmark to @: {e}"
            ))
        })?;
    runner
        .run(&RealCommandRunner::invocation(
            cwd,
            "jj",
            &["bookmark", "set", pr_bm, "-r", "@"],
        ))
        .map_err(|e| {
            CubeError::InvalidArgument(format!(
                "failed to advance `{pr_bm}` bookmark to @: {e}"
            ))
        })?;
    Ok(())
}

fn run_graph(_args: GraphArgs) -> Result<RunResult> {
    Err(CubeError::NotImplemented(
        "graph command is not implemented yet".to_string(),
    ))
}

fn run_doctor(_args: DoctorArgs) -> Result<RunResult> {
    Err(CubeError::NotImplemented(
        "doctor command is not implemented yet".to_string(),
    ))
}

fn next_workspace_id(prefix: &str, existing: &[String]) -> String {
    let mut max_n: u32 = 0;
    let mut found_any = false;
    for id in existing {
        if let Some(suffix) = id.strip_prefix(prefix)
            && let Ok(n) = suffix.parse::<u32>() {
                found_any = true;
                if n > max_n {
                    max_n = n;
                }
            }
    }
    let next = if found_any { max_n + 1 } else { 1 };
    format!("{prefix}{next:03}")
}

fn auto_create_workspace(
    runner: &dyn CommandRunner,
    repo_record: &RepoRecord,
    existing: &[crate::metadata::WorkspaceCandidate],
) -> Result<crate::metadata::WorkspaceCandidate> {
    let existing_ids: Vec<String> = existing.iter().map(|c| c.workspace_id.clone()).collect();
    let workspace_id = next_workspace_id(&repo_record.workspace_prefix, &existing_ids);
    let workspace_path = repo_record.workspace_root.join(&workspace_id);

    fs::create_dir_all(&repo_record.workspace_root).map_err(|e| CubeError::WorkspaceDirCreate {
        path: repo_record.workspace_root.clone(),
        source: e,
    })?;

    // Clone into a staging directory and only publish it under its final name
    // once it is fully populated, via an atomic rename. A clone interrupted
    // mid-flight then leaves only the dotted staging dir — which
    // `discover_workspaces` ignores (it doesn't match the workspace prefix) —
    // so a partially-populated tree can never be observed as a pool entry and
    // become a "broken-empty" husk (issue #845 part 2a). The repo lock is held
    // across the whole lease, so the staging name can't race a concurrent
    // create for this repo; clear any leftover from a prior interrupted run.
    let staging_path = repo_record
        .workspace_root
        .join(format!(".incoming-{workspace_id}"));
    if staging_path.exists() {
        fs::remove_dir_all(&staging_path).map_err(|source| CubeError::WorkspaceDirRemove {
            path: staging_path.clone(),
            source,
        })?;
    }

    let clone_source = match &repo_record.source {
        Some(source) if source.exists() => source.display().to_string(),
        _ => repo_record.origin.clone(),
    };

    runner.run(&CommandInvocation {
        cwd: repo_record.workspace_root.clone(),
        program: "jj".to_string(),
        args: vec![
            "git".to_string(),
            "clone".to_string(),
            "--colocate".to_string(),
            clone_source,
            staging_path.display().to_string(),
        ],
    })?;
    track_remote_bookmarks(runner, &staging_path, Some(repo_record.main_branch.as_str()))?;

    // Publish atomically. Staging and final live under the same workspace_root
    // (one filesystem), so the rename is atomic and the final path appears
    // only when the checkout is complete.
    fs::rename(&staging_path, &workspace_path).map_err(|source| CubeError::WorkspaceDirCreate {
        path: workspace_path.clone(),
        source,
    })?;

    Ok(crate::metadata::WorkspaceCandidate {
        workspace_id,
        workspace_path,
    })
}

/// Self-heal a broken-empty pool entry: a workspace directory that exists but
/// has neither `.jj/` nor `.git/`. Such a husk holds no recoverable work
/// (no commits, no working copy), so remove the directory and forget its
/// registry row, freeing the slot for a fresh clone. Called from the lease
/// path so a degraded pool self-repairs instead of blocking a lease
/// (issue #845 part 2b).
fn gc_broken_empty_workspace(
    store: &mut Store,
    database_path: Option<&Path>,
    repo: &str,
    workspace_id: &str,
    workspace_path: &Path,
) -> Result<()> {
    if workspace_path.exists() {
        fs::remove_dir_all(workspace_path).map_err(|source| CubeError::WorkspaceDirRemove {
            path: workspace_path.to_path_buf(),
            source,
        })?;
    }
    store.forget_workspace(repo, workspace_id)?;
    eprintln!(
        "warning: cube workspace `{repo}/{workspace_id}` had neither .git/ nor .jj/ \
         (broken-empty) at {}; removing the husk and provisioning a fresh workspace",
        workspace_path.display(),
    );
    audit!(
        database_path,
        "workspace.broken_empty_gc",
        repo = repo,
        workspace_id = workspace_id,
        workspace_path = workspace_path.display().to_string(),
    );
    Ok(())
}

fn run_setup_for_workspace(
    store: &Store,
    runner: &dyn CommandRunner,
    workspace: &WorkspaceRecord,
) -> Result<SetupReport> {
    let Some(config) = setup::read_setup_config(&workspace.workspace_path)? else {
        return Ok(SetupReport::empty());
    };
    let now = current_epoch_s()?;
    run_setup_engine(store, runner, workspace, &config, now)
}

fn format_lease_message(lease_message: &str, report: &SetupReport) -> String {
    if report.steps.is_empty() {
        return lease_message.to_string();
    }
    format!(
        "{lease_message} Setup: {} ran, {} skipped.",
        report.ran_count(),
        report.skipped_count()
    )
}

fn format_setup_message(workspace_id: &str, report: &SetupReport) -> String {
    if report.steps.is_empty() {
        return format!("No setup steps are configured for {workspace_id}.");
    }
    format!(
        "Setup complete for {workspace_id}: {} ran, {} skipped.",
        report.ran_count(),
        report.skipped_count()
    )
}

fn discover_workspaces(repo: &RepoRecord) -> Result<Vec<crate::metadata::WorkspaceCandidate>> {
    let mut candidates = Vec::new();
    if !repo.workspace_root.is_dir() {
        return Ok(candidates);
    }
    for entry in fs::read_dir(&repo.workspace_root).map_err(|e| CubeError::WorkspaceDirRead {
        path: repo.workspace_root.clone(),
        source: e,
    })? {
        let entry = entry.map_err(|e| CubeError::WorkspaceDirRead {
            path: repo.workspace_root.clone(),
            source: e,
        })?;
        let file_type = entry.file_type().map_err(|e| CubeError::WorkspaceDirRead {
            path: entry.path(),
            source: e,
        })?;
        if !file_type.is_dir() {
            continue;
        }

        let workspace_id = entry.file_name();
        let workspace_id = workspace_id.to_string_lossy().to_string();
        if !workspace_id.starts_with(&repo.workspace_prefix) {
            continue;
        }

        candidates.push(crate::metadata::WorkspaceCandidate {
            workspace_id,
            workspace_path: entry.path(),
        });
    }

    candidates.sort_by(|left, right| left.workspace_id.cmp(&right.workspace_id));
    Ok(candidates)
}

fn find_workspace_record(
    store: &mut Store,
    workspace_path: &Path,
) -> Result<Option<crate::metadata::WorkspaceRecord>> {
    if let Some(record) = store.get_workspace_by_path(workspace_path)? {
        if workspace_path_exists(&record) {
            return Ok(Some(record));
        }
        store.forget_workspace(&record.repo, &record.workspace_id)?;
    }

    for repo in store.list_repos()? {
        if workspace_path.starts_with(&repo.workspace_root) {
            let candidates = discover_workspaces(&repo)?;
            store.sync_workspaces(&repo.repo, &candidates)?;
        }
    }

    if let Some(record) = store.get_workspace_by_path(workspace_path)? {
        if workspace_path_exists(&record) {
            return Ok(Some(record));
        }
        store.forget_workspace(&record.repo, &record.workspace_id)?;
    }

    Ok(None)
}

/// List and optionally forget consumed `boss/exec_*` bookmarks and closed/merged
/// `pr/<n>` bookmarks in a workspace.
///
/// A `boss/exec_*` bookmark is "consumed" when its tip is reachable from `main`
/// (`bookmarks(glob:"boss/exec_*") & ::main`). A `pr/<n>` bookmark is eligible
/// for GC when its corresponding GitHub PR is in the MERGED or CLOSED state
/// (resolved via `gh pr view`; skipped silently when offline).
///
/// If `do_fetch` is true, runs `jj git fetch` first so `::main` reflects the
/// latest merged PRs. If `dry_run` is true, lists what would be forgotten
/// without acting.
///
/// Returns the names of bookmarks forgotten (or that would be forgotten on
/// dry-run). Failures are propagated to the caller; release-path callers
/// should treat them as warnings.
fn gc_workspace_bookmarks(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
    do_fetch: bool,
    dry_run: bool,
) -> Result<Vec<String>> {
    if do_fetch {
        run_jj(
            runner,
            database_path,
            &RealCommandRunner::invocation(workspace_path, "jj", &["git", "fetch"]),
        )?;
    }

    let output = run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            workspace_path,
            "jj",
            &[
                "log",
                "-r",
                "bookmarks(glob:\"boss/exec_*\") & ::main",
                "--no-graph",
                "-T",
                "bookmarks ++ \"\\n\"",
            ],
        ),
    )?;

    let mut bookmarks: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        output
            .split_whitespace()
            .filter(|s| s.starts_with("boss/exec_") && !s.contains('@'))
            .filter(|s| seen.insert(s.to_string()))
            .map(str::to_string)
            .collect()
    };

    // Also sweep pr/<n> bookmarks whose GitHub PR is closed or merged.
    bookmarks.extend(gc_collect_closed_pr_bookmarks(runner, database_path, workspace_path));

    if bookmarks.is_empty() || dry_run {
        return Ok(bookmarks);
    }

    let mut args: Vec<&str> = vec!["bookmark", "forget"];
    let bookmark_refs: Vec<&str> = bookmarks.iter().map(String::as_str).collect();
    args.extend_from_slice(&bookmark_refs);
    run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(workspace_path, "jj", &args),
    )?;

    Ok(bookmarks)
}

/// Collect local `pr/<n>` bookmarks in `workspace_path` whose GitHub PR is
/// MERGED or CLOSED. Returns an empty list when offline, the workspace has no
/// GitHub remote, or there are no `pr/*` bookmarks. Failures from `jj` or
/// `gh` are swallowed so this best-effort sweep never blocks the caller.
fn gc_collect_closed_pr_bookmarks(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
) -> Vec<String> {
    // Resolve the GitHub owner/repo slug from the workspace's jj remotes.
    let remote_output = match runner.run(&RealCommandRunner::invocation(
        workspace_path,
        "jj",
        &["git", "remote", "list"],
    )) {
        Ok(out) => out,
        Err(_) => return vec![],
    };
    let (_remote_name, owner_repo) = match parse_github_remote(&remote_output) {
        Some(r) => r,
        None => return vec![],
    };

    // Find all local pr/* bookmarks in the workspace.
    let bookmark_output = match run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            workspace_path,
            "jj",
            &[
                "log",
                "-r",
                "bookmarks(glob:\"pr/*\")",
                "--no-graph",
                "-T",
                "bookmarks ++ \"\\n\"",
            ],
        ),
    ) {
        Ok(out) => out,
        Err(_) => return vec![],
    };

    let pr_bookmarks: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        bookmark_output
            .split_whitespace()
            .filter(|s| pr_bookmark::is_pr_bookmark(s) && !s.contains('@'))
            .filter(|s| seen.insert(s.to_string()))
            .map(str::to_string)
            .collect()
    };

    if pr_bookmarks.is_empty() {
        return vec![];
    }

    // For each pr/<n> bookmark, query GitHub for the PR state. Skip silently
    // on network/auth failures so GC degrades gracefully when offline.
    pr_bookmarks
        .into_iter()
        .filter(|bookmark| {
            let Some(pr_num) = bookmark.strip_prefix("pr/") else {
                return false;
            };
            let state = match runner.run(&RealCommandRunner::invocation(
                workspace_path,
                "gh",
                &[
                    "pr", "view", pr_num, "-R", &owner_repo,
                    "--json", "state", "--jq", ".state",
                ],
            )) {
                Ok(out) => out,
                Err(_) => return false,
            };
            matches!(state.trim(), "MERGED" | "CLOSED")
        })
        .collect()
}

/// Update `last_pool_gc_at` and spawn a background thread to gc consumed
/// bookmarks across all free workspaces, at most once per 24 hours.
/// The timestamp is written BEFORE the thread is spawned so concurrent lease
/// calls within the same window skip redundant gc triggers.
fn maybe_trigger_pool_gc(
    store: &mut Store,
    database_path: Option<&Path>,
    now_epoch_s: i64,
) -> Result<()> {
    let last_gc = store.get_pool_metadata_i(POOL_GC_LAST_AT_KEY)?;
    let should_trigger = match last_gc {
        None => true,
        Some(last) => (now_epoch_s - last) >= AUTO_GC_INTERVAL_SECS,
    };
    if !should_trigger {
        return Ok(());
    }
    store.set_pool_metadata_i(POOL_GC_LAST_AT_KEY, now_epoch_s)?;
    let db_path_owned = database_path.map(Path::to_path_buf);
    std::thread::spawn(move || {
        run_pool_gc_background(db_path_owned);
    });
    Ok(())
}

fn run_pool_gc_background(database_path: Option<std::path::PathBuf>) {
    let store = match database_path.as_deref() {
        Some(p) => Store::open_at(p),
        None => Store::open_default(),
    };
    let Ok(store) = store else {
        eprintln!("cube: auto gc: failed to open store");
        return;
    };
    let records = match store.list_workspaces_filtered(&WorkspaceListFilter::default()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cube: auto gc: failed to list workspaces: {e}");
            return;
        }
    };
    let runner = RealCommandRunner;
    for record in &records {
        if record.state == WorkspaceState::Leased {
            continue;
        }
        if !workspace_path_exists(record) {
            continue;
        }
        if let Err(e) = gc_workspace_bookmarks(
            &runner,
            database_path.as_deref(),
            &record.workspace_path,
            true,
            false,
        ) {
            eprintln!(
                "cube: auto gc: {}: {e}",
                record.workspace_id,
            );
        }
    }

    let gc_config = config::load_config()
        .unwrap_or_default()
        .unhealthy_gc;
    let max_age_secs = gc_config.max_age_secs();
    if let Ok(now) = current_epoch_s() {
        gc_aged_unhealthy_workspaces(&runner, &store, database_path.as_deref(), now, max_age_secs);
    }

    gc_stale_workspace_logs(&store);
}

/// During pool GC, reset any non-leased free workspace that has been
/// continuously `dirty` or `conflicted` for longer than `max_age_secs`.
/// Emits a `workspace.unhealthy_gc_reset` audit event for each workspace
/// that is reclaimed so the discarded work is traceable.
fn gc_aged_unhealthy_workspaces(
    runner: &dyn CommandRunner,
    store: &Store,
    database_path: Option<&Path>,
    now_epoch_s: i64,
    max_age_secs: i64,
) {
    let records = match store.list_workspaces_filtered(&WorkspaceListFilter::default()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cube: unhealthy gc: failed to list workspaces: {e}");
            return;
        }
    };

    let threshold_epoch_s = now_epoch_s.saturating_sub(max_age_secs);

    for record in records {
        if record.state == WorkspaceState::Leased {
            continue;
        }
        let is_unhealthy = matches!(
            record.health_status,
            Some(WorkspaceHealth::Dirty) | Some(WorkspaceHealth::Conflicted)
        );
        if !is_unhealthy {
            continue;
        }
        let Some(unhealthy_since) = record.unhealthy_since_epoch_s else {
            continue;
        };
        if unhealthy_since > threshold_epoch_s {
            continue;
        }
        if !workspace_path_exists(&record) {
            continue;
        }

        // Re-check state: skip if the workspace was claimed between the list
        // and this point.
        let current_state = store
            .list_workspaces_filtered(&WorkspaceListFilter {
                repo: Some(&record.repo),
                workspace_id: Some(&record.workspace_id),
                ..Default::default()
            })
            .ok()
            .and_then(|mut v| v.pop())
            .map(|r| r.state);
        if current_state != Some(WorkspaceState::Free) {
            eprintln!(
                "cube: unhealthy gc: {} was claimed mid-pass, skipping",
                record.workspace_id,
            );
            continue;
        }

        let main_branch = match store.get_repo(&record.repo).ok().flatten() {
            Some(r) => r.main_branch,
            None => {
                eprintln!(
                    "cube: unhealthy gc: {}: repo {} not found, skipping",
                    record.workspace_id, record.repo,
                );
                continue;
            }
        };

        let prior_health = record
            .health_status
            .map(|h| h.as_str())
            .unwrap_or("unknown");
        let age_secs = now_epoch_s.saturating_sub(unhealthy_since);

        if let Err(e) = reset_workspace(
            runner,
            database_path,
            &record.workspace_path,
            &main_branch,
        ) {
            eprintln!(
                "cube: unhealthy gc: {}: reset failed: {e}",
                record.workspace_id,
            );
            continue;
        }

        match store.gc_clear_workspace_unhealthy_state(&record.repo, &record.workspace_id) {
            Ok(true) => {}
            Ok(false) => {
                eprintln!(
                    "cube: unhealthy gc: {}: claimed between reset and store update",
                    record.workspace_id,
                );
                continue;
            }
            Err(e) => {
                eprintln!(
                    "cube: unhealthy gc: {}: failed to clear store state: {e}",
                    record.workspace_id,
                );
                continue;
            }
        }

        audit!(
            database_path,
            "workspace.unhealthy_gc_reset",
            workspace_id = record.workspace_id,
            repo = record.repo,
            prior_health = prior_health,
            prior_holder = record.holder.as_deref(),
            prior_task = record.task.as_deref(),
            unhealthy_since_epoch_s = unhealthy_since,
            age_secs = age_secs,
        );

        eprintln!(
            "cube: unhealthy gc: reset {} (was {} for {}s)",
            record.workspace_id, prior_health, age_secs,
        );
    }
}

fn gc_stale_workspace_logs(store: &Store) {
    let data_dir = match paths::data_dir() {
        Ok(d) => d,
        Err(_) => return,
    };
    let logs_dir = paths::workspace_logs_dir_in(&data_dir);
    if !logs_dir.exists() {
        return;
    }
    let entries = match fs::read_dir(&logs_dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("cube: workspace logs gc: failed to read {}: {e}", logs_dir.display());
            return;
        }
    };
    let active_workspaces = match store.list_workspaces_filtered(&WorkspaceListFilter::default()) {
        Ok(w) => {
            w.iter()
                .map(|r| r.workspace_id.clone())
                .collect::<std::collections::HashSet<_>>()
        }
        Err(e) => {
            eprintln!("cube: workspace logs gc: failed to list workspaces: {e}");
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let workspace_id = match path.file_name().and_then(|n| n.to_str()) {
            Some(id) => id.to_string(),
            None => continue,
        };
        if !active_workspaces.contains(&workspace_id)
            && let Err(e) = fs::remove_dir_all(&path) {
                eprintln!(
                    "cube: workspace logs gc: failed to remove {}: {e}",
                    path.display()
                );
            }
    }
}

/// Markers delimiting the cube-managed block inside a workspace's local
/// `.git/info/exclude`. We rewrite only the region between them, leaving
/// any operator-added excludes untouched, and they make the provenance
/// of the patterns obvious to anyone reading the file.
const BOSS_INFRA_EXCLUDE_BEGIN: &str = "# >>> boss-internal infra (managed by cube) >>>";
const BOSS_INFRA_EXCLUDE_END: &str = "# <<< boss-internal infra (managed by cube) <<<";

/// Render the cube-managed exclude block for a workspace.
///
/// `/logs/<workspace-id>.log` is anchored to the single empty infra log
/// some host tooling drops at workspace-setup time (issue #1174) — named
/// exactly after the cube workspace — rather than blanket-ignoring
/// `logs/`, which a repo may legitimately track. `.boss/` is the engine's
/// own per-run scratch/log dir (e.g. the remote runner's `worker.log`),
/// which is never part of a deliverable.
fn render_boss_infra_exclude_block(workspace_id: &str) -> String {
    format!(
        "{BOSS_INFRA_EXCLUDE_BEGIN}\n\
         # Keeps Boss/host infra files out of the worker's jj snapshot so they\n\
         # never land in a PR (issue #1174). cube rewrites this block on every\n\
         # lease; edit patterns above/below it, not inside.\n\
         .boss/\n\
         /logs/{workspace_id}.log\n\
         {BOSS_INFRA_EXCLUDE_END}\n"
    )
}

/// Insert or replace the cube-managed block in an exclude-file body,
/// preserving everything outside the markers. Idempotent: a body already
/// carrying an identical block is returned byte-for-byte unchanged.
fn upsert_managed_exclude(existing: &str, block: &str) -> String {
    if let (Some(start), Some(end_marker)) = (
        existing.find(BOSS_INFRA_EXCLUDE_BEGIN),
        existing.find(BOSS_INFRA_EXCLUDE_END),
    ) {
        let end = end_marker + BOSS_INFRA_EXCLUDE_END.len();
        // Swallow the newline after the END marker so repeated rewrites
        // don't accumulate blank lines between the block and any tail.
        let tail_start = if existing[end..].starts_with('\n') {
            end + 1
        } else {
            end
        };
        format!("{}{block}{}", &existing[..start], &existing[tail_start..])
    } else {
        let mut out = String::from(existing);
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(block);
        out
    }
}

/// Ensure the colocated workspace's local git exclude file carries the
/// cube-managed Boss-infra ignore block, so jj never auto-snapshots those
/// infra files into the worker's working-copy commit (and thus its PR) —
/// defense-in-depth for issue #1174.
///
/// `.git/info/exclude` is the right home: it lives under `.git/` (never
/// committed, never shipped in a PR), is local to this one workspace, and
/// jj honors it for git-backed/colocated repos exactly like a tracked
/// `.gitignore`. Best-effort by design — a workspace without a colocated
/// `.git/` directory, or an unreadable/unwritable exclude file, is left
/// untouched rather than failing the lease.
fn ensure_boss_infra_excluded(workspace_path: &Path, workspace_id: &str) {
    let git_dir = workspace_path.join(".git");
    if !git_dir.is_dir() {
        // Non-colocated layout: no `info/exclude` jj would read. The guard
        // is defense-in-depth, so skip silently rather than warn.
        return;
    }
    let info_dir = git_dir.join("info");
    if let Err(e) = fs::create_dir_all(&info_dir) {
        eprintln!(
            "warning: cube could not create {} for the Boss-infra exclude guard: {e}",
            info_dir.display()
        );
        return;
    }
    let exclude_path = info_dir.join("exclude");
    let existing = match fs::read_to_string(&exclude_path) {
        Ok(body) => body,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            eprintln!(
                "warning: cube could not read {} for the Boss-infra exclude guard: {e}",
                exclude_path.display()
            );
            return;
        }
    };
    let next = upsert_managed_exclude(&existing, &render_boss_infra_exclude_block(workspace_id));
    if next == existing {
        return;
    }
    if let Err(e) = fs::write(&exclude_path, &next) {
        eprintln!(
            "warning: cube could not write {} for the Boss-infra exclude guard: {e}",
            exclude_path.display()
        );
    }
}

fn reset_workspace(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
    main_branch: &str,
) -> Result<()> {
    reset_workspace_guarded(runner, database_path, workspace_path, main_branch, None)
}

/// Resolve the GitHub remote name and `owner/repo` slug from `jj git remote
/// list` run inside the given workspace path.
fn resolve_github_remote_for_workspace(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
) -> Result<(String, String)> {
    let remote_output = run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(workspace_path, "jj", &["git", "remote", "list"]),
    )?;
    parse_github_remote(&remote_output).ok_or_else(|| {
        CubeError::InvalidArgument(format!(
            "could not detect a github.com remote from `jj git remote list` in {}:\n{remote_output}",
            workspace_path.display()
        ))
    })
}

/// Info returned from a successful `--resume_pr` positioning pass.
struct PrResumeInfo {
    pr_number: u64,
    head_branch: String,
}

/// Replace the normal `jj new <main>` reset with the PR-resume positioning
/// sequence: resolve github remote → fetch → resolve PR N's head via `gh` →
/// reconcile `pr/<n>` and head-branch bookmarks → `jj new pr/<n>`.
///
/// After this returns, `@` is a fresh empty commit ready to edit on top of
/// PR N's current head.
fn resume_workspace_on_pr(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
    pr_number: u64,
    prior_expired: Option<&crate::store::ExpiredLease>,
    main_branch: &str,
) -> Result<PrResumeInfo> {
    let (github_remote, owner_repo) =
        resolve_github_remote_for_workspace(runner, database_path, workspace_path)?;

    // Fetch from the GitHub remote — load-bearing for the cold path where the
    // PR branch has never been fetched into this workspace.
    run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            workspace_path,
            "jj",
            &["git", "fetch", "--remote", &github_remote],
        ),
    )?;

    // Guard: if this workspace was reclaimed from an expired lease, refuse to
    // reposition `@` when the prior holder left uncommitted work. Without this
    // check, `jj new pr/<n>` would snapshot those files into the new commit,
    // silently mixing them with the PR's content. Matches the guard in
    // `reset_workspace_guarded`.
    if let Some(prior) = prior_expired {
        let head_status = read_head_status(runner, database_path, workspace_path, main_branch)?;
        if !head_status.is_clean_on_main {
            audit!(
                database_path,
                "workspace.reset_refused_dirty",
                workspace_path = workspace_path.display().to_string(),
                main_branch = main_branch,
                head_change_id = head_status.head_change_id,
                head_is_empty = head_status.head_is_empty,
                head_parent_bookmarks = head_status.head_parent_bookmarks,
                prior_lease_id = prior.lease_id,
                prior_holder = prior.holder.as_deref(),
                prior_task = prior.task.as_deref(),
            );
            return Err(CubeError::LeaseExpiredWorkspaceDirty {
                workspace_path: workspace_path.to_path_buf(),
                prior_lease_id: prior.lease_id.clone(),
                prior_holder: prior
                    .holder
                    .clone()
                    .unwrap_or_else(|| "<unknown>".to_string()),
            });
        }
    }

    // Resolve PR N's current head from GitHub: state, head branch, and OID.
    let pr_n_str = pr_number.to_string();
    let pr_json = runner
        .run(&RealCommandRunner::invocation(
            workspace_path,
            "gh",
            &[
                "pr",
                "view",
                &pr_n_str,
                "-R",
                &owner_repo,
                "--json",
                "headRefName,headRefOid,state",
            ],
        ))
        .map_err(|e| {
            CubeError::InvalidArgument(format!(
                "failed to resolve PR {pr_number} in {owner_repo}: {e}"
            ))
        })?;

    let pr_info: serde_json::Value = serde_json::from_str(&pr_json)?;

    let state = pr_info
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN");
    if state == "MERGED" || state == "CLOSED" {
        return Err(CubeError::InvalidArgument(format!(
            "PR {pr_number} ({owner_repo}) is {state} — cannot resume on a non-open PR. \
             Use `cube workspace lease` without `--resume-pr` for a fresh task, or check \
             the PR number."
        )));
    }

    let head_branch = pr_info
        .get("headRefName")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            CubeError::InvalidArgument(format!(
                "PR {pr_number} ({owner_repo}) returned no headRefName from GitHub"
            ))
        })?
        .to_string();

    let pr_bm = pr_bookmark::pr_bookmark_name(pr_number);
    let remote_ref = format!("{head_branch}@{github_remote}");

    // Set pr/<n> to the fetched GitHub head (create-or-move, idempotent).
    // Works for both the warm path (reconciling an existing local bookmark) and
    // the cold path (creating it for the first time in this workspace).
    run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            workspace_path,
            "jj",
            &["bookmark", "set", &pr_bm, "-r", &remote_ref],
        ),
    )?;

    // Re-establish the local head-branch bookmark pointing at the fetched ref
    // so a later `cube pr push` has the branch name available.
    run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(
            workspace_path,
            "jj",
            &[
                "bookmark",
                "set",
                &head_branch,
                "-r",
                &remote_ref,
                "--allow-backwards",
            ],
        ),
    )?;

    // Land editable: fresh empty child commit on top of the PR head.
    run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(workspace_path, "jj", &["new", &pr_bm]),
    )?;

    Ok(PrResumeInfo {
        pr_number,
        head_branch,
    })
}

/// Variant of [`reset_workspace`] that refuses to run the destructive
/// `jj new <main>` step if the workspace's `@` still has the prior
/// lease holder's uncommitted work AND `prior_expired` says the lease
/// we just claimed was reclaimed-out-from-under that holder. Surfaces
/// [`CubeError::LeaseExpiredWorkspaceDirty`] so the lease handler can
/// abort cleanly instead of stomping on the still-active worker.
///
/// When `prior_expired` is `None` (normal release path, or a workspace
/// that was already `free`), the guard is a no-op and behavior matches
/// the original `reset_workspace`.
///
/// Every `jj` invocation here also writes an audit entry
/// (`workspace.jj_op`) so the next time someone reports "my `@`
/// moved", we can replay the log and prove or disprove a cube-side
/// reset.
fn reset_workspace_guarded(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
    main_branch: &str,
    prior_expired: Option<&crate::store::ExpiredLease>,
) -> Result<()> {
    audit_jj_op(database_path, workspace_path, "git", &["fetch"], prior_expired);
    run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(workspace_path, "jj", &["git", "fetch"]),
    )?;

    if let Some(prior) = prior_expired {
        let head_status = read_head_status(runner, database_path, workspace_path, main_branch)?;
        if !head_status.is_clean_on_main {
            audit!(
                database_path,
                "workspace.reset_refused_dirty",
                workspace_path = workspace_path.display().to_string(),
                main_branch = main_branch,
                head_change_id = head_status.head_change_id,
                head_is_empty = head_status.head_is_empty,
                head_parent_bookmarks = head_status.head_parent_bookmarks,
                prior_lease_id = prior.lease_id,
                prior_holder = prior.holder.as_deref(),
                prior_task = prior.task.as_deref(),
            );
            return Err(CubeError::LeaseExpiredWorkspaceDirty {
                workspace_path: workspace_path.to_path_buf(),
                prior_lease_id: prior.lease_id.clone(),
                prior_holder: prior
                    .holder
                    .clone()
                    .unwrap_or_else(|| "<unknown>".to_string()),
            });
        }
    }

    fast_forward_default_branch_to_origin(runner, database_path, workspace_path, main_branch, prior_expired)?;

    audit_jj_op(database_path, workspace_path, "new", &[main_branch], prior_expired);
    run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(workspace_path, "jj", &["new", main_branch]),
    )?;
    Ok(())
}

/// Fast-forward the workspace's local default bookmark to the
/// `<main>@origin` position established by the preceding `jj git fetch`,
/// so the subsequent `jj new <main>` — and any `jj new <main>` the
/// worker runs itself — branches from current origin rather than a
/// stale local base.
///
/// `jj git fetch` always updates the `<main>@origin` remote-tracking
/// bookmark, but it advances the *local* `<main>` bookmark only when
/// that bookmark is still tracking its remote and has not diverged. A
/// reused workspace whose local `<main>` fell out of tracking (or was
/// nudged by an earlier op) therefore keeps a days-old local `<main>`
/// even though `<main>@origin` is current — which is exactly how reused
/// workspaces cut PR branches from a stale base (spinyfin/mono#1232).
/// An explicit `jj bookmark set` to the remote-tracking target closes
/// that gap unconditionally. `--allow-backwards` is intentional: the
/// local default branch must mirror origin exactly, even in the rare
/// case it somehow sits ahead.
///
/// Tolerant of an unresolvable `<main>@origin` (a repo whose recorded
/// default branch has no matching remote bookmark): warn and continue,
/// leaving the prior local bookmark for `jj new <main>` to resolve,
/// rather than bricking the lease.
fn fast_forward_default_branch_to_origin(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
    main_branch: &str,
    prior_expired: Option<&crate::store::ExpiredLease>,
) -> Result<()> {
    let remote_target = format!("{main_branch}@origin");
    audit_jj_op(
        database_path,
        workspace_path,
        "bookmark-set",
        &[main_branch, &remote_target],
        prior_expired,
    );
    let invocation = RealCommandRunner::invocation(
        workspace_path,
        "jj",
        &[
            "bookmark",
            "set",
            main_branch,
            "-r",
            &remote_target,
            "--allow-backwards",
        ],
    );
    match run_jj(runner, database_path, &invocation) {
        Ok(_) => Ok(()),
        Err(err) if is_unresolved_remote_target(&err) => {
            eprintln!(
                "warning: cube could not fast-forward `{main_branch}` to `{remote_target}` \
                 in {}: the remote-tracking bookmark did not resolve. Leaving local \
                 `{main_branch}` in place; check the repo's recorded default branch.",
                workspace_path.display()
            );
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// Outcome of a pre-lease health check on a free workspace.
#[derive(Debug, Clone)]
enum WorkspaceHealthOutcome {
    /// Working copy is clean and no bookmarks are conflicted. Ready to use as-is.
    Clean,
    /// Working copy is clean, but one or more bookmarks are conflicted.
    /// Auto-repairable: forget the named bookmarks before resetting.
    ConflictedBookmarks(Vec<String>),
    /// Working copy has uncommitted changes from a prior worker session.
    /// Not safe to auto-repair — skip this workspace.
    DirtyWorkingCopy,
    /// Workspace directory exists but has neither `.jj/` nor `.git/`.
    /// The directory was likely wiped externally. Requires manual re-clone or
    /// force-release. Not safe to auto-repair without the source.
    BrokenEmpty,
}

/// Check the health of a free workspace by running `jj status`. Returns
/// [`WorkspaceHealthOutcome`] so the lease handler can decide whether to
/// skip, repair, or immediately use the workspace. Does not modify the
/// workspace.
fn check_workspace_health(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
) -> Result<WorkspaceHealthOutcome> {
    // Detect broken-empty workspaces by checking the directory state
    // directly rather than waiting for jj to report an error. A workspace
    // with neither .jj/ nor .git/ cannot be used or healed without a
    // full re-clone, so return early before spawning jj at all.
    if !workspace_path.join(".jj").is_dir() && !workspace_path.join(".git").is_dir() {
        return Ok(WorkspaceHealthOutcome::BrokenEmpty);
    }

    let output = run_jj(
        runner,
        database_path,
        &CommandInvocation {
            cwd: workspace_path.to_path_buf(),
            program: "jj".to_string(),
            args: vec!["status".to_string(), "--no-pager".to_string()],
        },
    )?;

    // "Working copy changes:" appears when jj has file-level changes staged
    // or present in the working copy. Its absence means the working copy
    // itself is clean (though bookmarks may still be conflicted).
    if output
        .lines()
        .any(|l| l.trim_start().starts_with("Working copy changes:"))
    {
        return Ok(WorkspaceHealthOutcome::DirtyWorkingCopy);
    }

    let conflicted = parse_conflicted_bookmarks_from_status(&output);
    if !conflicted.is_empty() {
        return Ok(WorkspaceHealthOutcome::ConflictedBookmarks(conflicted));
    }

    Ok(WorkspaceHealthOutcome::Clean)
}

/// Extract conflicted bookmark names from `jj status` output.
///
/// `jj status` includes a section like:
/// ```text
/// These bookmarks have conflicts:
///   fix-spawn-worker-pane-burst-crash
///   Use `jj bookmark list` to see details. ...
/// ```
/// The bookmark names are the indented lines before the "Use …" hint.
fn parse_conflicted_bookmarks_from_status(status: &str) -> Vec<String> {
    let mut in_section = false;
    let mut bookmarks = Vec::new();
    for line in status.lines() {
        if line.contains("These bookmarks have conflicts:") {
            in_section = true;
            continue;
        }
        if in_section {
            let trimmed = line.trim();
            if trimmed.is_empty() || !line.starts_with(' ') {
                break; // end of section
            }
            // Skip the advisory "Use `jj bookmark list`…" line.
            if trimmed.starts_with("Use `jj bookmark") {
                continue;
            }
            bookmarks.push(trimmed.to_string());
        }
    }
    bookmarks
}

/// Forget each conflicted bookmark so the workspace no longer reports
/// bookmark conflicts. Called at lease time when the health check finds
/// `ConflictedBookmarks` and the workspace is otherwise clean (working
/// copy empty). `jj bookmark forget` removes the local tracking state
/// without touching remote refs.
fn repair_conflicted_bookmarks(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
    bookmarks: &[String],
) -> Result<()> {
    for bookmark in bookmarks {
        audit!(
            database_path,
            "workspace.bookmark_forgotten",
            workspace_path = workspace_path.display().to_string(),
            bookmark = bookmark,
        );
        run_jj(
            runner,
            database_path,
            &CommandInvocation {
                cwd: workspace_path.to_path_buf(),
                program: "jj".to_string(),
                args: vec!["bookmark".to_string(), "forget".to_string(), bookmark.clone()],
            },
        )?;
    }
    Ok(())
}

/// Audit one of cube's own `jj` invocations against a leased
/// workspace. Pair-reads of this with the cube audit log let an
/// investigator answer "did cube move my `@`?" without guesswork —
/// every fetch/new/log/etc. cube runs lands here with the workspace
/// path, the verb, and (when relevant) the lease that just expired.
fn audit_jj_op(
    database_path: Option<&Path>,
    workspace_path: &Path,
    verb: &str,
    args: &[&str],
    prior_expired: Option<&crate::store::ExpiredLease>,
) {
    audit!(
        database_path,
        "workspace.jj_op",
        workspace_path = workspace_path.display().to_string(),
        verb = verb,
        args = args,
        prior_expired_lease_id = prior_expired.map(|e| e.lease_id.as_str()),
        prior_expired_holder = prior_expired.and_then(|e| e.holder.as_deref()),
    );
}

/// Snapshot the parts of `jj`'s view we need to tell apart "fresh
/// clean checkout on main" from "the prior worker left work here."
/// Empty + parent is main → safe to reset. Anything else → guard.
#[derive(Debug)]
struct HeadStatus {
    head_change_id: String,
    head_is_empty: bool,
    head_parent_bookmarks: String,
    is_clean_on_main: bool,
}

fn read_head_status(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
    main_branch: &str,
) -> Result<HeadStatus> {
    // Tab-separated so a bookmark name containing arbitrary chars
    // (jj allows slashes etc.) can't confuse the parser.
    let template = "change_id ++ \"\\t\" ++ empty ++ \"\\t\" ++ parents.map(|p| p.bookmarks().join(\",\")).join(\";\")";
    let output = run_jj(
        runner,
        database_path,
        &CommandInvocation {
            cwd: workspace_path.to_path_buf(),
            program: "jj".to_string(),
            args: vec![
                "log".to_string(),
                "--no-graph".to_string(),
                "-r".to_string(),
                "@".to_string(),
                "-T".to_string(),
                template.to_string(),
            ],
        },
    )?;
    let trimmed = output.trim();
    let mut parts = trimmed.split('\t');
    let head_change_id = parts.next().unwrap_or("").to_string();
    let head_is_empty = parts.next().unwrap_or("false").eq_ignore_ascii_case("true");
    let head_parent_bookmarks = parts.next().unwrap_or("").to_string();
    // Treat "@ is empty and its parent is on `main`" as a clean reset
    // candidate. The bookmark list is `;`-separated by parent (jj's @
    // can have multiple parents post-merge), and each entry is a
    // comma-separated list of bookmarks on that parent.
    let parent_is_main = head_parent_bookmarks
        .split(';')
        .flat_map(|p| p.split(','))
        .any(|b| b.trim() == main_branch);
    let is_clean_on_main = head_is_empty && parent_is_main;
    Ok(HeadStatus {
        head_change_id,
        head_is_empty,
        head_parent_bookmarks,
        is_clean_on_main,
    })
}

/// Run a `jj` command against a workspace, transparently recovering
/// from a stale working copy, op-log divergence, or a missing jj repo
/// alongside an existing git repo. If the underlying command fails with
/// `working copy is stale` or `seems to be a sibling`, runs
/// `jj workspace update-stale` once and retries. If it fails with
/// `there is no jj repo` and a `.git/` directory is present, runs
/// `jj git init --colocate` once and retries. If it fails with
/// `there is no jj repo` and neither `.git/` nor `.jj/` is present,
/// surfaces a clear `NoAvailableWorkspace` error naming the broken
/// workspace path instead of the raw jj message. Other failures and
/// non-`jj` invocations pass through untouched.
fn run_jj(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    invocation: &CommandInvocation,
) -> Result<String> {
    match runner.run(invocation) {
        Ok(out) => Ok(out),
        Err(err) => {
            // Sibling heal: workspace has .git but no .jj — colocate-init jj.
            if jj_needs_colocate_init(&err, &invocation.cwd) {
                eprintln!(
                    "cube: initialised jj on existing git workspace {}",
                    invocation.cwd.display()
                );
                let init = RealCommandRunner::invocation(
                    &invocation.cwd,
                    "jj",
                    &["git", "init", "--colocate"],
                );
                if runner.run(&init).is_err() {
                    return Err(err);
                }
                audit!(
                    database_path,
                    "workspace.jj_colocate_initialised",
                    workspace_path = invocation.cwd.display().to_string(),
                    program = invocation.program,
                    args = invocation.args,
                );
                return match runner.run(invocation) {
                    Ok(out) => Ok(out),
                    Err(_) => Err(err),
                };
            }

            // Broken-empty: workspace has neither .jj/ nor .git/ — the
            // directory was likely wiped externally. Surface a clear error
            // naming the path and what's missing rather than the raw jj
            // "no jj repo" message, which gives no actionable information.
            if jj_workspace_broken_empty(&err, &invocation.cwd) {
                return Err(CubeError::NoAvailableWorkspace(format!(
                    "workspace at `{}` has neither .jj/ nor .git/ (broken-empty): \
                     the workspace directory exists but no jj or git repository was found. \
                     Re-clone manually or `cube workspace force-release` and retry.",
                    invocation.cwd.display()
                )));
            }

            let Some(recovery_kind) = jj_update_stale_recovery_kind(&err) else {
                return Err(err);
            };
            if recovery_kind == "workspace.op_diverged_recovered" {
                eprintln!(
                    "cube: jj op-log diverged on {}; running `jj workspace update-stale` to recover",
                    invocation.cwd.display()
                );
            }
            let update_stale = RealCommandRunner::invocation(
                &invocation.cwd,
                "jj",
                &["workspace", "update-stale"],
            );
            if let Err(update_err) = runner.run(&update_stale) {
                return Err(CubeError::StaleRecoveryFailed {
                    workspace_path: invocation.cwd.clone(),
                    cause: format!("jj workspace update-stale failed: {update_err}"),
                });
            }
            audit!(
                database_path,
                recovery_kind,
                workspace_path = invocation.cwd.display().to_string(),
                program = invocation.program,
                args = invocation.args,
            );
            match runner.run(invocation) {
                Ok(out) => Ok(out),
                Err(retry_err) => Err(CubeError::StaleRecoveryFailed {
                    workspace_path: invocation.cwd.clone(),
                    cause: format!("retry after update-stale failed: {retry_err}"),
                }),
            }
        }
    }
}

/// Returns `true` when the error is `jj`'s "no jj repo" diagnostic AND a
/// `.git/` directory exists at `cwd`, meaning `jj git init --colocate` can
/// recover the workspace. Returns `false` for all other errors or when
/// `.git/` is absent (truly broken state — do not paper over it).
fn jj_needs_colocate_init(err: &CubeError, cwd: &Path) -> bool {
    let CubeError::CommandFailed { program, stderr, .. } = err else {
        return false;
    };
    if program != "jj" {
        return false;
    }
    let lower = stderr.to_lowercase();
    lower.contains(JJ_NO_JJ_REPO_SIGNATURE) && cwd.join(".git").is_dir()
}

/// Returns `true` when the error is `jj`'s "no jj repo" diagnostic AND
/// neither `.jj/` nor `.git/` exists at `cwd`. This is the shorter error
/// variant jj emits when the directory has no repo at all (as opposed to
/// the longer hint-bearing form jj emits when `.git/` is present without
/// `.jj/`). Directory state is checked directly rather than by inspecting
/// jj's error text — the text is brittle; the directory check is not.
fn jj_workspace_broken_empty(err: &CubeError, cwd: &Path) -> bool {
    let CubeError::CommandFailed { program, stderr, .. } = err else {
        return false;
    };
    if program != "jj" {
        return false;
    }
    let lower = stderr.to_lowercase();
    lower.contains(JJ_NO_JJ_REPO_SIGNATURE)
        && !cwd.join(".jj").is_dir()
        && !cwd.join(".git").is_dir()
}

/// Returns the audit event name if the error is one that `jj workspace
/// update-stale` can fix, or `None` if the error is unrelated.
fn jj_update_stale_recovery_kind(err: &CubeError) -> Option<&'static str> {
    let CubeError::CommandFailed { program, stderr, .. } = err else {
        return None;
    };
    if program != "jj" {
        return None;
    }
    let lower = stderr.to_lowercase();
    if lower.contains(JJ_STALE_SIGNATURE) {
        return Some("workspace.stale_recovered");
    }
    if lower.contains(JJ_OP_DIVERGED_SIGNATURE) {
        return Some("workspace.op_diverged_recovered");
    }
    None
}


fn current_workspace_commit(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
) -> Result<String> {
    run_jj(
        runner,
        database_path,
        &CommandInvocation {
            cwd: workspace_path.to_path_buf(),
            program: "jj".to_string(),
            args: vec![
                "log".to_string(),
                "--no-graph".to_string(),
                "-r".to_string(),
                "@".to_string(),
                "-T".to_string(),
                "commit_id.short()".to_string(),
            ],
        },
    )
}

fn current_change_identity(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
) -> Result<ChangeIdentity> {
    let output = run_jj(
        runner,
        database_path,
        &CommandInvocation {
            cwd: workspace_path.to_path_buf(),
            program: "jj".to_string(),
            args: vec![
                "log".to_string(),
                "--no-graph".to_string(),
                "-r".to_string(),
                "@".to_string(),
                "-T".to_string(),
                "change_id ++ \"\\n\" ++ commit_id.short()".to_string(),
            ],
        },
    )?;
    let mut lines = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let jj_change_id = lines
        .next()
        .ok_or_else(|| {
            CubeError::InvalidArgument("jj change query did not return a change id".to_string())
        })?
        .to_string();
    let head_commit = lines
        .next()
        .ok_or_else(|| {
            CubeError::InvalidArgument("jj change query did not return a head commit".to_string())
        })?
        .to_string();
    Ok(ChangeIdentity {
        jj_change_id,
        head_commit,
    })
}

fn workspace_path_exists(record: &crate::metadata::WorkspaceRecord) -> bool {
    record.workspace_path.is_dir()
}

fn cleanup_workspace_logs(workspace_id: &str) -> Result<()> {
    if let Ok(logs_path) = paths::workspace_logs_path(workspace_id) {
        match fs::remove_dir_all(&logs_path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(CubeError::Io(err)),
        }
    }
    Ok(())
}

/// Summary of a workspace row touched by the missing-directory reconciler.
/// Surfaced through `cube workspace list --json` and also fed to per-row
/// audit events so the operator has a paper trail.
#[derive(Debug, Clone, Serialize)]
struct ReconciledRow {
    repo: String,
    workspace_id: String,
    workspace_path: PathBuf,
    prior_state: WorkspaceState,
    lease_id: Option<String>,
    holder: Option<String>,
    lease_expires_at_epoch_s: Option<i64>,
}

impl ReconciledRow {
    fn from_record(record: &WorkspaceRecord) -> Self {
        Self {
            repo: record.repo.clone(),
            workspace_id: record.workspace_id.clone(),
            workspace_path: record.workspace_path.clone(),
            prior_state: record.state,
            lease_id: record.lease_id.clone(),
            holder: record.holder.clone(),
            lease_expires_at_epoch_s: record.lease_expires_at_epoch_s,
        }
    }
}

/// What `reconcile_missing_workspaces` did in one pass. `removed` is rows
/// that were actually evicted from the registry (free-and-missing, plus
/// leased-and-missing rows whose lease had already expired). `held` is
/// leased rows whose directory is gone but whose lease is still within
/// its TTL — surfaced with a stderr warning and an audit event, but left
/// in place so the operator can decide whether to `force-release`.
#[derive(Debug, Default, Clone, Serialize)]
struct ReconcileReport {
    removed: Vec<ReconciledRow>,
    held: Vec<ReconciledRow>,
}

impl ReconcileReport {
    fn merge(&mut self, other: ReconcileReport) {
        self.removed.extend(other.removed);
        self.held.extend(other.held);
    }
}

/// Reconcile dangling registry rows whose on-disk workspace directory has
/// been deleted out from under cube — for one specific repo.
///
/// **The caller must already hold the per-repo `RepoLock`.** Use the
/// public [`reconcile_missing_workspaces`] wrapper from call sites that
/// don't already own the lock.
///
/// Decision matrix per row:
/// - `state=free`, dir missing → forget the row (a stray directory was
///   deleted manually; the registry entry is just stale).
/// - `state=leased`, dir missing, lease TTL elapsed → force-release the
///   lease and forget the row. The worker that held it presumably
///   crashed or had its workspace wiped; the lease has already aged out.
/// - `state=leased`, dir missing, lease still active → leave the row
///   alone but warn loudly. We can't know whether the holder is mid-setup
///   or genuinely dead, so we defer to the operator (who can then
///   `cube workspace force-release <id>` and re-run).
fn reconcile_missing_workspaces_in_repo(
    store: &mut Store,
    database_path: Option<&Path>,
    repo: &str,
    now_epoch_s: i64,
) -> Result<ReconcileReport> {
    let workspaces = store.list_workspaces_filtered(&WorkspaceListFilter {
        repo: Some(repo),
        ..Default::default()
    })?;
    let mut report = ReconcileReport::default();
    for row in workspaces {
        if workspace_path_exists(&row) {
            continue;
        }
        match row.state {
            WorkspaceState::Free => {
                let summary = ReconciledRow::from_record(&row);
                store.forget_workspace(&row.repo, &row.workspace_id)?;
                eprintln!(
                    "warning: cube workspace `{}/{}` directory is missing at {}; \
                     removing the dangling registry row",
                    row.repo,
                    row.workspace_id,
                    row.workspace_path.display(),
                );
                audit!(
                    database_path,
                    "workspace.dir_missing_reconciled",
                    repo = row.repo,
                    workspace_id = row.workspace_id,
                    workspace_path = row.workspace_path.display().to_string(),
                    prior_state = row.state.as_str(),
                );
                report.removed.push(summary);
            }
            WorkspaceState::Leased => {
                // No expiry recorded → treat as still active; we have no
                // basis to evict a lease that pre-dates the TTL field.
                let lease_active = row
                    .lease_expires_at_epoch_s
                    .map(|exp| exp > now_epoch_s)
                    .unwrap_or(true);
                if lease_active {
                    eprintln!(
                        "warning: cube workspace `{}/{}` directory is missing at {} but its \
                         lease is still active (held by {}); run `cube workspace force-release \
                         {}` to reclaim",
                        row.repo,
                        row.workspace_id,
                        row.workspace_path.display(),
                        row.holder.as_deref().unwrap_or("<unknown>"),
                        row.workspace_id,
                    );
                    audit!(
                        database_path,
                        "workspace.dir_missing_held",
                        repo = row.repo,
                        workspace_id = row.workspace_id,
                        workspace_path = row.workspace_path.display().to_string(),
                        lease_id = row.lease_id,
                        holder = row.holder,
                        lease_expires_at_epoch_s = row.lease_expires_at_epoch_s,
                    );
                    report.held.push(ReconciledRow::from_record(&row));
                } else {
                    let summary = ReconciledRow::from_record(&row);
                    if let Some(lease_id) = row.lease_id.clone() {
                        let _ = store.force_release_lease(&lease_id, Some("dir_missing"))?;
                    }
                    store.forget_workspace(&row.repo, &row.workspace_id)?;
                    eprintln!(
                        "warning: cube workspace `{}/{}` directory is missing at {} and its \
                         lease has expired (was held by {}); force-releasing and removing the \
                         dangling registry row",
                        row.repo,
                        row.workspace_id,
                        row.workspace_path.display(),
                        row.holder.as_deref().unwrap_or("<unknown>"),
                    );
                    audit!(
                        database_path,
                        "workspace.dir_missing_reconciled",
                        repo = row.repo,
                        workspace_id = row.workspace_id,
                        workspace_path = row.workspace_path.display().to_string(),
                        prior_state = row.state.as_str(),
                        lease_id = row.lease_id,
                        holder = row.holder,
                    );
                    report.removed.push(summary);
                }
            }
        }
    }
    Ok(report)
}

/// Reconcile dangling registry rows across all repos (or a single repo
/// when `repo_filter` is set). Acquires the per-repo `RepoLock` for each
/// repo that has at least one drifted row, so this is safe to call from
/// commands that don't already hold a lock.
fn reconcile_missing_workspaces(
    store: &mut Store,
    database_path: Option<&Path>,
    repo_filter: Option<&str>,
    now_epoch_s: i64,
) -> Result<ReconcileReport> {
    let workspaces = store.list_workspaces_filtered(&WorkspaceListFilter {
        repo: repo_filter,
        ..Default::default()
    })?;
    let mut repos: Vec<String> = workspaces
        .iter()
        .filter(|ws| !workspace_path_exists(ws))
        .map(|ws| ws.repo.clone())
        .collect();
    repos.sort();
    repos.dedup();

    let mut report = ReconcileReport::default();
    for repo in repos {
        let _lock = RepoLock::acquire(&repo_lock_path(&repo, database_path)?)?;
        let sub = reconcile_missing_workspaces_in_repo(store, database_path, &repo, now_epoch_s)?;
        report.merge(sub);
    }
    Ok(report)
}

/// Returns the human-readable effective status string for a workspace,
/// combining the lease state with the last-known health status. Free
/// workspaces with a recorded health issue show `free-dirty` or
/// `free-conflicted` so operators can see at a glance which slots are
/// usable without `cd`-ing into each one.
fn effective_state_display(record: &WorkspaceRecord) -> String {
    match record.state {
        WorkspaceState::Leased => "leased".to_string(),
        WorkspaceState::Free => match record.health_status {
            Some(WorkspaceHealth::Dirty) => "free-dirty".to_string(),
            Some(WorkspaceHealth::Conflicted) => "free-conflicted".to_string(),
            _ => "free".to_string(),
        },
    }
}

fn format_workspace_list(records: &[WorkspaceRecord]) -> String {
    if records.is_empty() {
        return "No workspaces match.".to_string();
    }

    let names: Vec<String> = records
        .iter()
        .map(|r| format!("{}/{}", r.repo, r.workspace_id))
        .collect();
    let paths: Vec<String> = records
        .iter()
        .map(|r| abbreviate_path(&r.workspace_path))
        .collect();
    let effective_states: Vec<String> = records.iter().map(effective_state_display).collect();
    let name_w = names.iter().map(|s| s.len()).max().unwrap_or(0);
    let state_w = effective_states
        .iter()
        .map(|s| s.len())
        .max()
        .unwrap_or(0);

    let label_w = "holder".len();
    let dim = Style::new().dim();
    let mut lines = Vec::with_capacity(records.len());
    for (((record, name), path), eff_state) in
        records.iter().zip(&names).zip(&paths).zip(&effective_states)
    {
        let name_pad = format!("{name:<name_w$}");
        let state_pad = format!("{eff_state:<state_w$}");
        let state_styled = match record.state {
            WorkspaceState::Free => style(state_pad).green(),
            WorkspaceState::Leased => style(state_pad).yellow(),
        };
        lines.push(format!(
            "{}  {}  {}",
            style(name_pad).cyan().bold(),
            state_styled,
            dim.apply_to(path),
        ));

        if record.state == WorkspaceState::Leased {
            if let Some(holder) = &record.holder {
                lines.push(format!(
                    "    {}  {}",
                    dim.apply_to(format!("{:<label_w$}", "holder")),
                    holder,
                ));
            }
            if let Some(task) = &record.task {
                lines.push(format!(
                    "    {}  {}",
                    dim.apply_to(format!("{:<label_w$}", "task")),
                    task,
                ));
            }
            if let Some(lease) = &record.lease_id {
                lines.push(format!(
                    "    {}  {}",
                    dim.apply_to(format!("{:<label_w$}", "lease")),
                    dim.apply_to(lease),
                ));
            }
        }
    }
    lines.join("\n")
}

fn human_workspace_detail(record: &crate::metadata::WorkspaceRecord, jj_status: &str) -> String {
    let dim = Style::new().dim();
    let mut lines = vec![
        format!("{} {}", dim.apply_to("repo:"), record.repo),
        format!(
            "{} {}",
            dim.apply_to("workspace_id:"),
            style(&record.workspace_id).cyan().bold(),
        ),
        format!(
            "{} {}",
            dim.apply_to("workspace_path:"),
            abbreviate_path(&record.workspace_path),
        ),
        format!("{} {}", dim.apply_to("state:"), style_state(record.state),),
    ];
    if let Some(lease_id) = &record.lease_id {
        lines.push(format!(
            "{} {}",
            dim.apply_to("lease_id:"),
            dim.apply_to(lease_id),
        ));
    }
    if let Some(holder) = &record.holder {
        lines.push(format!("{} {holder}", dim.apply_to("holder:")));
    }
    if let Some(task) = &record.task {
        lines.push(format!("{} {task}", dim.apply_to("task:")));
    }
    if let Some(head_commit) = &record.head_commit {
        lines.push(format!(
            "{} {}",
            dim.apply_to("head_commit:"),
            dim.apply_to(head_commit),
        ));
    }
    lines.push(dim.apply_to("jj_status:").to_string());
    lines.push(jj_status.to_string());
    lines.join("\n")
}

fn style_state(state: WorkspaceState) -> console::StyledObject<&'static str> {
    match state {
        WorkspaceState::Free => style(state.as_str()).green(),
        WorkspaceState::Leased => style(state.as_str()).yellow(),
    }
}

fn abbreviate_path(p: &Path) -> String {
    let s = p.display().to_string();
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if !home.is_empty() {
            if s == home.as_ref() {
                return "~".to_string();
            }
            if let Some(rest) = s.strip_prefix(home.as_ref())
                && rest.starts_with('/')
            {
                return format!("~{rest}");
            }
        }
    }
    s
}

fn holder_identity() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string());
    format!("{user}@{host}:{}", std::process::id())
}

fn resolve_release_lease(
    store: &mut Store,
    workspace: Option<String>,
    lease: Option<String>,
    repo: Option<String>,
) -> Result<String> {
    if let Some(lease) = lease {
        return Ok(lease);
    }
    let workspace_id = workspace.ok_or_else(|| {
        CubeError::InvalidArgument(
            "release requires a workspace id positional or --lease".to_string(),
        )
    })?;
    let matches = store.list_workspaces_filtered(&WorkspaceListFilter {
        repo: repo.as_deref(),
        workspace_id: Some(&workspace_id),
        ..Default::default()
    })?;
    match matches.as_slice() {
        [] => Err(CubeError::WorkspaceNotFound(workspace_id)),
        [single] => single.lease_id.clone().ok_or_else(|| {
            CubeError::InvalidArgument(format!(
                "workspace `{}/{}` is not currently leased",
                single.repo, single.workspace_id
            ))
        }),
        many => {
            let repos = many
                .iter()
                .map(|r| r.repo.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(CubeError::InvalidArgument(format!(
                "workspace id `{workspace_id}` matches multiple repos ({repos}); disambiguate with --repo"
            )))
        }
    }
}

fn repo_lock_path(repo: &str, database_path: Option<&Path>) -> Result<PathBuf> {
    match database_path.and_then(Path::parent) {
        Some(parent) => Ok(paths::repo_lock_path_in(parent, repo)),
        None => paths::repo_lock_path(repo),
    }
}

fn current_epoch_s() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| CubeError::Io(io::Error::other(e)))?
        .as_secs() as i64)
}

fn stack_command_name(command: &StackCommand) -> &'static str {
    match command {
        StackCommand::Rebase { .. } => "rebase",
    }
}

fn pr_command_name(command: &PrCommand) -> &'static str {
    match command {
        PrCommand::Ensure(_) => "ensure",
        PrCommand::Push(_) => "push",
        PrCommand::Sync { .. } => "sync",
        PrCommand::Merge { .. } => "merge",
    }
}

fn format_repo_list(records: &[RepoRecord]) -> String {
    if records.is_empty() {
        return "No repos configured.".to_string();
    }
    let dim = Style::new().dim();
    let name_w = records.iter().map(|r| r.repo.len()).max().unwrap_or(0);
    let root_w = records
        .iter()
        .map(|r| abbreviate_path(&r.workspace_root).len())
        .max()
        .unwrap_or(0);
    records
        .iter()
        .map(|r| {
            let name_pad = format!("{:<name_w$}", r.repo);
            let root = abbreviate_path(&r.workspace_root);
            let root_pad = format!("{root:<root_w$}");
            format!(
                "{}  {}  {} {} {} {}",
                style(name_pad).cyan().bold(),
                dim.apply_to(root_pad),
                dim.apply_to("branch"),
                r.main_branch,
                dim.apply_to("prefix"),
                r.workspace_prefix,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn human_repo_detail(record: &RepoRecord) -> String {
    let dim = Style::new().dim();
    let mut lines = vec![
        format!(
            "{} {}",
            dim.apply_to("repo:"),
            style(&record.repo).cyan().bold(),
        ),
        format!("{} {}", dim.apply_to("origin:"), record.origin),
        format!("{} {}", dim.apply_to("main_branch:"), record.main_branch),
        format!(
            "{} {}",
            dim.apply_to("workspace_root:"),
            abbreviate_path(&record.workspace_root),
        ),
        format!(
            "{} {}",
            dim.apply_to("workspace_prefix:"),
            record.workspace_prefix,
        ),
    ];
    if let Some(source) = &record.source {
        lines.push(format!(
            "{} {}",
            dim.apply_to("source:"),
            abbreviate_path(source),
        ));
    }
    lines.join("\n")
}

fn human_change_detail(record: &ChangeRecord) -> String {
    let dim = Style::new().dim();
    let mut lines = vec![
        format!(
            "{} {}",
            dim.apply_to("change_id:"),
            style(&record.change_id).cyan().bold(),
        ),
        format!("{} {}", dim.apply_to("repo:"), record.repo),
        format!(
            "{} {}",
            dim.apply_to("workspace_path:"),
            abbreviate_path(&record.workspace_path),
        ),
        format!("{} {}", dim.apply_to("title:"), record.title),
        format!(
            "{} {}",
            dim.apply_to("jj_change_id:"),
            dim.apply_to(&record.jj_change_id),
        ),
        format!(
            "{} {}",
            dim.apply_to("head_commit:"),
            dim.apply_to(&record.head_commit),
        ),
    ];
    if let Some(parent_change_id) = &record.parent_change_id {
        lines.push(format!(
            "{} {}",
            dim.apply_to("parent_change_id:"),
            parent_change_id,
        ));
    }
    lines.push(format!(
        "{} {}",
        dim.apply_to("created_at_epoch_s:"),
        record.created_at_epoch_s,
    ));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::process::ExitCode;

    use clap::Parser;
    use serde_json::json;
    use tempfile::TempDir;

    use crate::cli::{Cli, Command};
    use crate::command_runner::{CommandInvocation, CommandRunner};

    use super::{
        BOSS_INFRA_EXCLUDE_BEGIN, BOSS_INFRA_EXCLUDE_END, CubeError, POOL_GC_LAST_AT_KEY,
        RepoEnsureDefaults, Result, current_epoch_s, ensure_boss_infra_excluded,
        gc_aged_unhealthy_workspaces, is_stdin_path, render_boss_infra_exclude_block,
        resolve_body_file, run_with_context, run_with_dependencies, upsert_managed_exclude,
    };

    fn with_database_path() -> (TempDir, std::path::PathBuf) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let database_path = tempdir.path().join("state.db");
        (tempdir, database_path)
    }

    fn repo_ensure_defaults(tempdir: &TempDir) -> RepoEnsureDefaults {
        RepoEnsureDefaults {
            repo_root: tempdir.path().join("repos"),
            workspace_root: tempdir.path().join("workspaces"),
        }
    }

    #[test]
    fn repo_add_and_info_round_trip() {
        let (_tempdir, database_path) = with_database_path();

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            "/tmp/workspaces",
            "--workspace-prefix",
            "mono-agent-",
            "--source",
            "/tmp/mono",
        ]);
        let add_result = run_with_dependencies(add, Some(&database_path), &FakeRunner::default())
            .expect("repo add should succeed");
        assert_eq!(add_result.message, "Registered repo `mono`.");
        assert_eq!(add_result.payload["repo"]["repo"], "mono");

        let info = Cli::parse_from(["cube", "repo", "info", "mono"]);
        let info_result = run_with_dependencies(info, Some(&database_path), &FakeRunner::default())
            .expect("repo info should succeed");
        assert_eq!(
            info_result.payload["repo"]["workspace_prefix"],
            "mono-agent-"
        );
        assert_eq!(info_result.payload["repo"]["source"], "/tmp/mono");
    }

    #[test]
    fn repo_list_reports_empty_store() {
        let (_tempdir, database_path) = with_database_path();

        let cli = Cli::parse_from(["cube", "repo", "list"]);
        let result = run_with_dependencies(cli, Some(&database_path), &FakeRunner::default())
            .expect("repo list should succeed");

        assert_eq!(result.message, "No repos configured.");
        assert_eq!(result.payload["repos"], json!([]));
    }

    #[test]
    fn repo_commands_report_missing_repo_with_specific_exit_code() {
        let (_tempdir, database_path) = with_database_path();

        let cli = Cli::parse_from(["cube", "repo", "info", "mono"]);
        let error = run_with_dependencies(cli, Some(&database_path), &FakeRunner::default())
            .expect_err("repo info should fail when the repo is unknown");

        assert!(matches!(error, CubeError::RepoNotFound(_)));
        assert_eq!(error.exit_code(), ExitCode::from(3));
    }

    #[test]
    fn repo_ensure_reuses_existing_repo_by_origin() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        std::fs::create_dir_all(defaults.repo_root.join("mono")).expect("source dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &defaults.workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
            "--source",
            &defaults.repo_root.join("mono").display().to_string(),
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let ensure = Cli::parse_from([
            "cube",
            "repo",
            "ensure",
            "--origin",
            "git@github.com:spinyfin/mono.git",
        ]);
        let result = run_with_context(
            ensure,
            Some(&database_path),
            &FakeRunner::default(),
            Some(&defaults),
            None,
        )
        .expect("ensure");

        assert_eq!(result.message, "Ensured repo `mono`.");
        assert_eq!(result.payload["repo_id"], "mono");
        assert_eq!(
            result.payload["repo"]["workspace_root"],
            defaults.workspace_root.display().to_string()
        );
        assert_eq!(
            result.payload["repo"]["source"],
            defaults.repo_root.join("mono").display().to_string()
        );
    }

    #[test]
    fn repo_ensure_materializes_missing_source_for_existing_repo() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        let source_path = defaults.repo_root.join("mono");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &defaults.workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
            "--source",
            &source_path.display().to_string(),
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let runner = FakeRunner::new(vec![
            ExpectedCommand::ls_remote_symref(
                defaults.repo_root.clone(),
                "git@github.com:spinyfin/mono.git",
                "main",
            ),
            ExpectedCommand::ok(
                defaults.repo_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "git@github.com:spinyfin/mono.git",
                    &source_path.display().to_string(),
                ],
                "",
            )
            .creating_dir(source_path.clone()),
            ExpectedCommand::ok(
                source_path.clone(),
                "jj",
                &["bookmark", "track", "main@origin"],
                "",
            ),
            ExpectedCommand::no_such_remote_bookmark(
                source_path.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
            ),
        ]);

        let ensure = Cli::parse_from([
            "cube",
            "repo",
            "ensure",
            "--origin",
            "git@github.com:spinyfin/mono.git",
        ]);
        let result = run_with_context(ensure, Some(&database_path), &runner, Some(&defaults), None)
            .expect("ensure");

        assert_eq!(result.message, "Ensured repo `mono`.");
        assert_eq!(
            result.payload["repo"]["source"],
            source_path.display().to_string()
        );
        runner.assert_exhausted();
    }

    #[test]
    fn repo_ensure_infers_repo_and_materializes_missing_source() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        let source_path = defaults.repo_root.join("mono");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ls_remote_symref(
                defaults.repo_root.clone(),
                "git@github.com:spinyfin/mono.git",
                "main",
            ),
            ExpectedCommand::ok(
                defaults.repo_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "git@github.com:spinyfin/mono.git",
                    &source_path.display().to_string(),
                ],
                "",
            )
            .creating_dir(source_path.clone()),
            ExpectedCommand::ok(
                source_path.clone(),
                "jj",
                &["bookmark", "track", "main@origin"],
                "",
            ),
            ExpectedCommand::no_such_remote_bookmark(
                source_path.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
            ),
        ]);

        let ensure = Cli::parse_from([
            "cube",
            "repo",
            "ensure",
            "--origin",
            "git@github.com:spinyfin/mono.git",
        ]);
        let result = run_with_context(ensure, Some(&database_path), &runner, Some(&defaults), None)
            .expect("ensure");

        assert_eq!(result.message, "Ensured repo `mono`.");
        assert_eq!(result.payload["repo_id"], "mono");
        assert_eq!(result.payload["repo"]["workspace_prefix"], "mono-agent-");
        assert_eq!(
            result.payload["repo"]["workspace_root"],
            defaults.workspace_root.display().to_string()
        );
        assert_eq!(
            result.payload["repo"]["source"],
            source_path.display().to_string()
        );
        assert!(defaults.workspace_root.is_dir());
        runner.assert_exhausted();
    }

    fn resolver_config(
        name: &str,
        origin_pattern: &str,
        clone_command: Option<&str>,
    ) -> crate::config::CubeConfig {
        crate::config::CubeConfig {
            repo_resolvers: vec![crate::config::RepoResolver {
                name: name.to_string(),
                origin_pattern: origin_pattern.to_string(),
                clone_command: clone_command.map(str::to_string),
            }],
            unhealthy_gc: Default::default(),
        }
    }

    #[test]
    fn repo_ensure_by_name_uses_resolver_clone_command() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        let source_path = defaults.repo_root.join("frontend-api");

        // "true" stands in for `mint` — it exists on PATH so the which-check
        // passes. The clone command is the {name}-substituted resolver string.
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                defaults.repo_root.clone(),
                "true",
                &["clone", "frontend-api"],
                "",
            )
            .creating_dir(source_path.clone()),
            ExpectedCommand::ok(
                source_path.clone(),
                "jj",
                &["git", "init", "--colocate"],
                "",
            ),
            // This LinkedIn repo's default branch is `master`, so detection
            // must record `main_branch = "master"` rather than the old default.
            ExpectedCommand::ls_remote_symref(
                source_path.clone(),
                "org-127256988@github.com:linkedin-multiproduct/frontend-api.git",
                "master",
            ),
        ]);

        let ensure = Cli::parse_from(["cube", "repo", "ensure", "frontend-api"]);
        let result = run_with_context(
            ensure,
            Some(&database_path),
            &runner,
            Some(&defaults),
            Some(resolver_config(
                "mint",
                "org-127256988@github.com:linkedin-multiproduct/{name}.git",
                Some("true clone {name}"),
            )),
        )
        .expect("ensure");

        assert_eq!(result.message, "Ensured repo `frontend-api`.");
        assert_eq!(result.payload["repo_id"], "frontend-api");
        assert_eq!(
            result.payload["repo"]["origin"],
            "org-127256988@github.com:linkedin-multiproduct/frontend-api.git"
        );
        assert_eq!(
            result.payload["repo"]["clone_command"],
            "true clone frontend-api"
        );
        assert_eq!(result.payload["repo"]["main_branch"], "master");
        runner.assert_exhausted();
    }

    #[test]
    fn repo_ensure_by_name_uses_resolver_without_clone_command() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        let source_path = defaults.repo_root.join("widget");
        let origin = "git@github.example.com:corp/widget.git";

        let runner = FakeRunner::new(vec![
            ExpectedCommand::ls_remote_symref(defaults.repo_root.clone(), origin, "main"),
            ExpectedCommand::ok(
                defaults.repo_root.clone(),
                "jj",
                &["git", "clone", origin, &source_path.display().to_string()],
                "",
            )
            .creating_dir(source_path.clone()),
            ExpectedCommand::ok(
                source_path.clone(),
                "jj",
                &["bookmark", "track", "main@origin"],
                "",
            ),
            ExpectedCommand::no_such_remote_bookmark(
                source_path.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
            ),
        ]);

        let ensure = Cli::parse_from(["cube", "repo", "ensure", "widget"]);
        let result = run_with_context(
            ensure,
            Some(&database_path),
            &runner,
            Some(&defaults),
            Some(resolver_config(
                "corp-github",
                "git@github.example.com:corp/{name}.git",
                None,
            )),
        )
        .expect("ensure");

        assert_eq!(result.message, "Ensured repo `widget`.");
        assert_eq!(result.payload["repo"]["clone_command"], serde_json::Value::Null);
        runner.assert_exhausted();
    }

    #[test]
    fn repo_ensure_by_name_slug_match_is_noop_and_beats_resolver() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        // Pre-register `mono` with an on-disk source so materialize is a no-op.
        std::fs::create_dir_all(defaults.repo_root.join("mono")).expect("source dir");
        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &defaults.workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
            "--source",
            &defaults.repo_root.join("mono").display().to_string(),
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("add");

        // A resolver is configured, but the slug match (step 1) wins first, so
        // no clone command runs at all.
        let ensure = Cli::parse_from(["cube", "repo", "ensure", "mono"]);
        let result = run_with_context(
            ensure,
            Some(&database_path),
            &FakeRunner::default(),
            Some(&defaults),
            Some(resolver_config(
                "mint",
                "org-1@github.com:linkedin-multiproduct/{name}.git",
                Some("true clone {name}"),
            )),
        )
        .expect("ensure");

        assert_eq!(result.message, "Ensured repo `mono`.");
        assert_eq!(
            result.payload["repo"]["origin"],
            "git@github.com:spinyfin/mono.git"
        );
    }

    #[test]
    fn repo_ensure_by_name_github_fallback() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        let source_path = defaults.repo_root.join("mono");
        let origin = "git@github.com:spinyfin/mono.git";

        let runner = FakeRunner::new(vec![
            ExpectedCommand::ls_remote_symref(defaults.repo_root.clone(), origin, "main"),
            ExpectedCommand::ok(
                defaults.repo_root.clone(),
                "jj",
                &["git", "clone", origin, &source_path.display().to_string()],
                "",
            )
            .creating_dir(source_path.clone()),
            ExpectedCommand::ok(
                source_path.clone(),
                "jj",
                &["bookmark", "track", "main@origin"],
                "",
            ),
            ExpectedCommand::no_such_remote_bookmark(
                source_path.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
            ),
        ]);

        // No resolvers configured, so the `<org>/<name>` fallback synthesises a
        // github.com origin and clones it with plain `jj git clone`.
        let ensure = Cli::parse_from(["cube", "repo", "ensure", "spinyfin/mono"]);
        let result = run_with_context(
            ensure,
            Some(&database_path),
            &runner,
            Some(&defaults),
            Some(crate::config::CubeConfig::default()),
        )
        .expect("ensure");

        assert_eq!(result.message, "Ensured repo `mono`.");
        assert_eq!(result.payload["repo"]["origin"], origin);
        // The remote symref reported `main`, so the recorded default matches.
        assert_eq!(result.payload["repo"]["main_branch"], "main");
        runner.assert_exhausted();
    }

    /// When the remote's default branch is `master`, materialization must
    /// record `main_branch = "master"` (not the historical `main` default) by
    /// reading the `git ls-remote --symref` symref. `master@origin` already sits
    /// in the conventional candidate set, so the tracking order is unchanged.
    #[test]
    fn repo_ensure_detects_master_default_branch() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        let source_path = defaults.repo_root.join("legacy");
        let origin = "git@github.com:spinyfin/legacy.git";

        let runner = FakeRunner::new(vec![
            ExpectedCommand::ls_remote_symref(defaults.repo_root.clone(), origin, "master"),
            ExpectedCommand::ok(
                defaults.repo_root.clone(),
                "jj",
                &["git", "clone", origin, &source_path.display().to_string()],
                "",
            )
            .creating_dir(source_path.clone()),
            ExpectedCommand::no_such_remote_bookmark(
                source_path.clone(),
                "jj",
                &["bookmark", "track", "main@origin"],
            ),
            ExpectedCommand::ok(
                source_path.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
                "",
            ),
        ]);

        let ensure = Cli::parse_from(["cube", "repo", "ensure", "spinyfin/legacy"]);
        let result = run_with_context(
            ensure,
            Some(&database_path),
            &runner,
            Some(&defaults),
            Some(crate::config::CubeConfig::default()),
        )
        .expect("ensure");

        assert_eq!(result.payload["repo"]["main_branch"], "master");
        runner.assert_exhausted();
    }

    /// A non-conventional default branch (`develop`) must be recorded as
    /// `main_branch` AND promoted to a local tracking bookmark, since neither
    /// `main` nor `master` would otherwise give the lease's `jj new <branch>` a
    /// bookmark to resolve. The detected branch is appended after the two
    /// conventional names in the tracking sequence.
    #[test]
    fn repo_ensure_detects_and_tracks_nonconventional_default_branch() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        let source_path = defaults.repo_root.join("trunkish");
        let origin = "git@github.com:spinyfin/trunkish.git";

        let runner = FakeRunner::new(vec![
            ExpectedCommand::ls_remote_symref(defaults.repo_root.clone(), origin, "develop"),
            ExpectedCommand::ok(
                defaults.repo_root.clone(),
                "jj",
                &["git", "clone", origin, &source_path.display().to_string()],
                "",
            )
            .creating_dir(source_path.clone()),
            ExpectedCommand::no_such_remote_bookmark(
                source_path.clone(),
                "jj",
                &["bookmark", "track", "main@origin"],
            ),
            ExpectedCommand::no_such_remote_bookmark(
                source_path.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
            ),
            ExpectedCommand::ok(
                source_path.clone(),
                "jj",
                &["bookmark", "track", "develop@origin"],
                "",
            ),
        ]);

        let ensure = Cli::parse_from(["cube", "repo", "ensure", "spinyfin/trunkish"]);
        let result = run_with_context(
            ensure,
            Some(&database_path),
            &runner,
            Some(&defaults),
            Some(crate::config::CubeConfig::default()),
        )
        .expect("ensure");

        assert_eq!(result.payload["repo"]["main_branch"], "develop");
        runner.assert_exhausted();
    }

    /// If default-branch detection fails (git missing, network/auth error,
    /// unparseable output), materialization must not abort — it falls back to
    /// the historical `main` default and still tracks the conventional
    /// bookmarks.
    #[test]
    fn repo_ensure_falls_back_to_main_when_detection_fails() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        let source_path = defaults.repo_root.join("mono");
        let origin = "git@github.com:spinyfin/mono.git";

        let runner = FakeRunner::new(vec![
            ExpectedCommand {
                cwd: defaults.repo_root.clone(),
                program: "git".to_string(),
                args: vec![
                    "ls-remote".to_string(),
                    "--symref".to_string(),
                    origin.to_string(),
                    "HEAD".to_string(),
                ],
                result: Err(CubeError::CommandFailed {
                    program: "git".to_string(),
                    args: Vec::new(),
                    status: Some(128),
                    stderr: "fatal: could not read from remote repository".to_string(),
                }),
                creates_dir: None,
            },
            ExpectedCommand::ok(
                defaults.repo_root.clone(),
                "jj",
                &["git", "clone", origin, &source_path.display().to_string()],
                "",
            )
            .creating_dir(source_path.clone()),
            ExpectedCommand::ok(
                source_path.clone(),
                "jj",
                &["bookmark", "track", "main@origin"],
                "",
            ),
            ExpectedCommand::no_such_remote_bookmark(
                source_path.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
            ),
        ]);

        let ensure = Cli::parse_from(["cube", "repo", "ensure", "spinyfin/mono"]);
        let result = run_with_context(
            ensure,
            Some(&database_path),
            &runner,
            Some(&defaults),
            Some(crate::config::CubeConfig::default()),
        )
        .expect("ensure");

        assert_eq!(result.payload["repo"]["main_branch"], "main");
        runner.assert_exhausted();
    }

    #[test]
    fn parse_symref_default_branch_reads_head_symref() {
        let out = "ref: refs/heads/master\tHEAD\n\
                   0123456789abcdef0123456789abcdef01234567\tHEAD";
        assert_eq!(
            super::parse_symref_default_branch(out),
            Some("master".to_string())
        );
    }

    #[test]
    fn parse_symref_default_branch_handles_nonconventional_name() {
        let out = "ref: refs/heads/develop\tHEAD\ndeadbeef\tHEAD";
        assert_eq!(
            super::parse_symref_default_branch(out),
            Some("develop".to_string())
        );
    }

    #[test]
    fn parse_symref_default_branch_returns_none_without_symref_line() {
        // Some transports omit the `ref:` line entirely (only the sha/HEAD line).
        let out = "0123456789abcdef0123456789abcdef01234567\tHEAD";
        assert_eq!(super::parse_symref_default_branch(out), None);
        assert_eq!(super::parse_symref_default_branch(""), None);
    }

    #[test]
    fn repo_ensure_by_name_no_match_errors_with_chain() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);

        // A bare single-segment name with no resolvers and no slug: every step
        // fails, so the error should narrate all three.
        let ensure = Cli::parse_from(["cube", "repo", "ensure", "bduff"]);
        let err = run_with_context(
            ensure,
            Some(&database_path),
            &FakeRunner::default(),
            Some(&defaults),
            Some(crate::config::CubeConfig::default()),
        )
        .expect_err("should fail when nothing resolves");

        let msg = err.to_string();
        assert!(msg.contains("could not resolve repo `bduff`"), "{msg}");
        assert!(msg.contains("registered slug"), "{msg}");
        assert!(msg.contains("no `repo-resolvers`"), "{msg}");
        assert!(msg.contains("GitHub `<org>/<name>`"), "{msg}");
    }

    #[test]
    fn repo_ensure_resolver_clone_command_missing_binary_errors() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);

        let ensure = Cli::parse_from(["cube", "repo", "ensure", "frontend-api"]);
        let err = run_with_context(
            ensure,
            Some(&database_path),
            &FakeRunner::default(),
            Some(&defaults),
            Some(resolver_config(
                "mint",
                "org-1@github.com:linkedin-multiproduct/{name}.git",
                Some("this-binary-does-not-exist-cube-test clone {name}"),
            )),
        )
        .expect_err("should fail when clone command binary is missing");

        let msg = err.to_string();
        assert!(
            msg.contains("this-binary-does-not-exist-cube-test"),
            "error should name the missing binary: {msg}"
        );
        assert!(msg.contains("not on PATH"), "error should mention PATH: {msg}");
        assert!(
            msg.contains("resolver"),
            "error should reference the resolver config: {msg}"
        );
    }

    #[test]
    fn repo_ensure_accepts_auth_prefixed_url_when_plain_stored() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        std::fs::create_dir_all(defaults.repo_root.join("bduff")).expect("source dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "bduff",
            "--origin",
            "git@github.com:linkedin-sandbox/bduff.git",
            "--workspace-root",
            &defaults.workspace_root.display().to_string(),
            "--workspace-prefix",
            "bduff-agent-",
            "--source",
            &defaults.repo_root.join("bduff").display().to_string(),
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo add");

        let ensure = Cli::parse_from([
            "cube",
            "repo",
            "ensure",
            "--origin",
            "org-132020694@github.com:linkedin-sandbox/bduff.git",
        ]);
        let result = run_with_context(
            ensure,
            Some(&database_path),
            &FakeRunner::default(),
            Some(&defaults),
            None,
        )
        .expect("ensure with auth-prefixed URL should succeed");

        assert_eq!(result.payload["repo_id"], "bduff");
    }

    #[test]
    fn repo_ensure_accepts_plain_url_when_auth_prefixed_stored() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        std::fs::create_dir_all(defaults.repo_root.join("bduff")).expect("source dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "bduff",
            "--origin",
            "org-132020694@github.com:linkedin-sandbox/bduff.git",
            "--workspace-root",
            &defaults.workspace_root.display().to_string(),
            "--workspace-prefix",
            "bduff-agent-",
            "--source",
            &defaults.repo_root.join("bduff").display().to_string(),
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo add");

        let ensure = Cli::parse_from([
            "cube",
            "repo",
            "ensure",
            "--origin",
            "git@github.com:linkedin-sandbox/bduff.git",
        ]);
        let result = run_with_context(
            ensure,
            Some(&database_path),
            &FakeRunner::default(),
            Some(&defaults),
            None,
        )
        .expect("ensure with plain URL should succeed when auth-prefixed is stored");

        assert_eq!(result.payload["repo_id"], "bduff");
    }

    #[test]
    fn repo_ensure_accepts_scp_url_when_ssh_scheme_stored() {
        // Reproduces the ci-infra user report: stored as ssh://, ensured as SCP-style.
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        std::fs::create_dir_all(defaults.repo_root.join("ci-infra")).expect("source dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "ci-infra",
            "--origin",
            "ssh://org-132020694@github.com/linkedin-eng/ci-infra.git",
            "--workspace-root",
            &defaults.workspace_root.display().to_string(),
            "--workspace-prefix",
            "ci-infra-agent-",
            "--source",
            &defaults.repo_root.join("ci-infra").display().to_string(),
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo add");

        let ensure = Cli::parse_from([
            "cube",
            "repo",
            "ensure",
            "--origin",
            "git@github.com:linkedin-eng/ci-infra.git",
        ]);
        let result = run_with_context(
            ensure,
            Some(&database_path),
            &FakeRunner::default(),
            Some(&defaults),
            None,
        )
        .expect("ensure with SCP URL should succeed when ssh:// form is stored");

        assert_eq!(result.payload["repo_id"], "ci-infra");
    }

    #[test]
    fn repo_ensure_accepts_ssh_scheme_when_scp_stored() {
        // Inverse direction: stored as SCP, ensured as ssh://.
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        std::fs::create_dir_all(defaults.repo_root.join("ci-infra")).expect("source dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "ci-infra",
            "--origin",
            "git@github.com:linkedin-eng/ci-infra.git",
            "--workspace-root",
            &defaults.workspace_root.display().to_string(),
            "--workspace-prefix",
            "ci-infra-agent-",
            "--source",
            &defaults.repo_root.join("ci-infra").display().to_string(),
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo add");

        let ensure = Cli::parse_from([
            "cube",
            "repo",
            "ensure",
            "--origin",
            "ssh://org-132020694@github.com/linkedin-eng/ci-infra.git",
        ]);
        let result = run_with_context(
            ensure,
            Some(&database_path),
            &FakeRunner::default(),
            Some(&defaults),
            None,
        )
        .expect("ensure with ssh:// URL should succeed when SCP form is stored");

        assert_eq!(result.payload["repo_id"], "ci-infra");
    }

    #[test]
    fn repo_ensure_still_rejects_different_path() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        std::fs::create_dir_all(defaults.repo_root.join("bduff")).expect("source dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "bduff",
            "--origin",
            "git@github.com:linkedin-sandbox/bduff.git",
            "--workspace-root",
            &defaults.workspace_root.display().to_string(),
            "--workspace-prefix",
            "bduff-agent-",
            "--source",
            &defaults.repo_root.join("bduff").display().to_string(),
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo add");

        let ensure = Cli::parse_from([
            "cube",
            "repo",
            "ensure",
            "--origin",
            "git@github.com:linkedin-eng/bduff.git",
        ]);
        let err = run_with_context(
            ensure,
            Some(&database_path),
            &FakeRunner::default(),
            Some(&defaults),
            None,
        )
        .expect_err("ensure with different path should fail");

        assert!(matches!(err, CubeError::InvalidArgument(_)));
        let msg = err.to_string();
        assert!(msg.contains("cannot ensure"), "error: {msg}");
    }

    #[test]
    fn repo_ensure_accepts_bare_slug_when_already_configured() {
        // Reproduces issue #837: the repo is registered with an SSO-scoped
        // SSH origin, but Boss ensures it with only the product's bare
        // `owner/name` slug. Cube must not synthesise an origin from the slug
        // and assert it matches — a slug that names the configured repo is a
        // no-op success.
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        std::fs::create_dir_all(defaults.repo_root.join("dev-infra")).expect("source dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "dev-infra",
            "--origin",
            "ssh://org-127256988@github.com/linkedin-multiproduct/dev-infra.git",
            "--workspace-root",
            &defaults.workspace_root.display().to_string(),
            "--workspace-prefix",
            "dev-infra-agent-",
            "--source",
            &defaults.repo_root.join("dev-infra").display().to_string(),
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo add");

        let ensure = Cli::parse_from([
            "cube",
            "repo",
            "ensure",
            "--origin",
            "linkedin-multiproduct/dev-infra",
        ]);
        let result = run_with_context(
            ensure,
            Some(&database_path),
            &FakeRunner::default(),
            Some(&defaults),
            None,
        )
        .expect("ensure with a bare slug should succeed when the repo is configured");

        assert_eq!(result.payload["repo_id"], "dev-infra");
        // The registered origin — not the slug — is returned.
        assert_eq!(
            result.payload["repo"]["origin"],
            "ssh://org-127256988@github.com/linkedin-multiproduct/dev-infra.git"
        );
    }

    #[test]
    fn repo_ensure_rejects_bare_slug_with_different_owner() {
        // A slug whose owner differs from the registered origin's path is a
        // genuine conflict, not a no-op — keep rejecting it.
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        std::fs::create_dir_all(defaults.repo_root.join("dev-infra")).expect("source dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "dev-infra",
            "--origin",
            "ssh://org-127256988@github.com/linkedin-multiproduct/dev-infra.git",
            "--workspace-root",
            &defaults.workspace_root.display().to_string(),
            "--workspace-prefix",
            "dev-infra-agent-",
            "--source",
            &defaults.repo_root.join("dev-infra").display().to_string(),
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo add");

        let ensure = Cli::parse_from([
            "cube",
            "repo",
            "ensure",
            "--origin",
            "some-other-org/dev-infra",
        ]);
        let err = run_with_context(
            ensure,
            Some(&database_path),
            &FakeRunner::default(),
            Some(&defaults),
            None,
        )
        .expect_err("ensure with a mismatched slug should fail");

        assert!(matches!(err, CubeError::InvalidArgument(_)));
        assert!(err.to_string().contains("cannot ensure"), "error: {err}");
    }

    #[test]
    fn graph_arguments_parse_from_docs_shape() {
        let cli = Cli::parse_from(["cube", "graph", "--workspace", "/tmp/mono-agent-004"]);

        match cli.command {
            Command::Graph(graph) => {
                assert_eq!(graph.workspace.as_deref(), Some("/tmp/mono-agent-004"))
            }
            _ => panic!("expected graph command"),
        }
    }

    #[test]
    fn workspace_lease_claims_first_free_workspace_and_records_head_commit() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");
        std::fs::create_dir_all(workspace_root.join("mono-agent-005").join(".jj")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let first_path = workspace_root.join("mono-agent-004");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(first_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(first_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(first_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(first_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                first_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);

        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "implement cube",
        ]);
        let result = run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

        assert_eq!(
            result.payload["workspace"]["workspace_id"],
            "mono-agent-004"
        );
        assert_eq!(
            result.payload["workspace"]["workspace_path"],
            first_path.display().to_string()
        );
        assert_eq!(result.payload["workspace"]["head_commit"], "abc1234");
        runner.assert_exhausted();
    }

    /// The on-lease fast-forward (`jj bookmark set <main> -r <main>@origin`)
    /// must run between `jj git fetch` and `jj new <main>` so the worker
    /// always branches from current origin and never a stale local base
    /// (spinyfin/mono#1232).
    #[test]
    fn workspace_lease_fast_forwards_default_branch_to_origin() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

        run_with_dependencies(add_repo_cli(&workspace_root), Some(&database_path), &FakeRunner::default())
            .expect("repo");

        let first_path = workspace_root.join("mono-agent-004");
        // lease_runner_for already encodes the fetch → bookmark-set → new
        // ordering; assert_exhausted fails if the fast-forward step is
        // skipped or reordered.
        let runner = lease_runner_for(&first_path, "abc1234");
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "ff"]),
            Some(&database_path),
            &runner,
        )
        .expect("lease");
        runner.assert_exhausted();
    }

    /// A repo whose recorded default branch has no matching `@origin`
    /// remote bookmark must not brick the lease: the fast-forward degrades
    /// to a warning and `jj new <main>` still runs against the local
    /// bookmark, preserving the historical behavior for that edge case.
    #[test]
    fn workspace_lease_tolerates_unresolvable_origin_default_branch() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

        run_with_dependencies(add_repo_cli(&workspace_root), Some(&database_path), &FakeRunner::default())
            .expect("repo");

        let first_path = workspace_root.join("mono-agent-004");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(first_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(first_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::revision_doesnt_exist(
                first_path.clone(),
                "jj",
                &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            ),
            ExpectedCommand::ok(first_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                first_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "tolerate"]),
            Some(&database_path),
            &runner,
        )
        .expect("lease should succeed despite unresolvable origin default branch");
        assert_eq!(result.payload["workspace"]["head_commit"], "abc1234");
        runner.assert_exhausted();
    }

    #[test]
    fn workspace_lease_auto_creates_when_pool_is_empty() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        // intentionally no workspace dirs created up front

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let new_path = workspace_root.join("mono-agent-001");
        let staging = workspace_root.join(".incoming-mono-agent-001");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "--colocate",
                    "git@github.com:spinyfin/mono.git",
                    &staging.display().to_string(),
                ],
                "",
            )
            .creating_dir(staging.clone()),
            ExpectedCommand::ok(
                staging.clone(),
                "jj",
                &["bookmark", "track", "main@origin"],
                "",
            ),
            ExpectedCommand::no_such_remote_bookmark(
                staging.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
            ),
            ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                new_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);

        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "auto-create demo",
        ]);
        let result = run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

        assert_eq!(
            result.payload["workspace"]["workspace_id"],
            "mono-agent-001"
        );
        assert_eq!(result.payload["workspace"]["state"], "leased");
        assert_eq!(result.payload["workspace"]["task"], "auto-create demo");
        assert_eq!(result.payload["workspace"]["head_commit"], "abc1234");
        runner.assert_exhausted();
    }

    /// Regression for spinyfin/mono#696. A fresh `jj git clone` only
    /// populates `<branch>@origin` (remote) bookmarks; there is no
    /// local `main`/`master` for `jj new <main>` to resolve. The lease
    /// must promote those remote bookmarks to local tracking right
    /// after cloning. The expectation here pins the two specific
    /// `jj bookmark track <name>@origin` calls (one for `main`, one
    /// for `master`) between clone and reset — exercising the
    /// `--main-branch master` shape where `main@origin` doesn't exist
    /// in the remote and the per-branch error must be swallowed.
    /// `workspace_lease_tracks_main_origin_after_fresh_clone` covers
    /// the reverse `--main-branch main` shape.
    #[test]
    fn workspace_lease_tracks_master_origin_after_fresh_clone() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "legacy",
            "--origin",
            "git@github.com:spinyfin/legacy.git",
            "--main-branch",
            "master",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "legacy-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let new_path = workspace_root.join("legacy-agent-001");
        let staging = workspace_root.join(".incoming-legacy-agent-001");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "--colocate",
                    "git@github.com:spinyfin/legacy.git",
                    &staging.display().to_string(),
                ],
                "",
            )
            .creating_dir(staging.clone()),
            ExpectedCommand::no_such_remote_bookmark(
                staging.clone(),
                "jj",
                &["bookmark", "track", "main@origin"],
            ),
            ExpectedCommand::ok(
                staging.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
                "",
            ),
            ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["bookmark", "set", "master", "-r", "master@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["new", "master"], ""),
            ExpectedCommand::ok(
                new_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "fee1dead",
            ),
        ]);

        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "legacy",
            "--task",
            "fresh-clone master default",
        ]);
        let result = run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

        assert_eq!(
            result.payload["workspace"]["workspace_id"],
            "legacy-agent-001"
        );
        assert_eq!(result.payload["workspace"]["state"], "leased");
        runner.assert_exhausted();
    }

    /// Counterpart to `workspace_lease_tracks_master_origin_after_fresh_clone`:
    /// the same two-call sequence (`bookmark track main@origin`,
    /// `bookmark track master@origin`) but for a repo whose default
    /// branch is `main`. Pins that `master@origin` not existing in the
    /// remote is swallowed, not propagated. This is the common case in
    /// modern repos.
    #[test]
    fn workspace_lease_tracks_main_origin_after_fresh_clone() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--main-branch",
            "main",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let new_path = workspace_root.join("mono-agent-001");
        let staging = workspace_root.join(".incoming-mono-agent-001");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "--colocate",
                    "git@github.com:spinyfin/mono.git",
                    &staging.display().to_string(),
                ],
                "",
            )
            .creating_dir(staging.clone()),
            ExpectedCommand::ok(
                staging.clone(),
                "jj",
                &["bookmark", "track", "main@origin"],
                "",
            ),
            ExpectedCommand::no_such_remote_bookmark(
                staging.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
            ),
            ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                new_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "cafef00d",
            ),
        ]);

        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "fresh-clone main default",
        ]);
        let result = run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

        assert_eq!(
            result.payload["workspace"]["workspace_id"],
            "mono-agent-001"
        );
        assert_eq!(result.payload["workspace"]["state"], "leased");
        runner.assert_exhausted();
    }

    /// Pins that cube does NOT promote remote bookmarks other than
    /// `main`/`master` after a fresh clone. Real repos accumulate
    /// long-lived `feature/*`, `release/*`, and
    /// `gh-readonly-queue/*` remote refs; tracking all of them
    /// pollutes the local bookmark namespace and slows
    /// `jj log` / `jj bookmark list` in every leased workspace. The
    /// FakeRunner's strict call-sequence match enforces this: any
    /// stray `bookmark track <other>@origin` invocation would fail
    /// the assertion. If cube ever regresses to `glob:*@origin` (or
    /// to tracking individual non-default branches), this test fails.
    #[test]
    fn workspace_lease_does_not_track_non_default_origin_bookmarks() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let new_path = workspace_root.join("mono-agent-001");
        let staging = workspace_root.join(".incoming-mono-agent-001");
        // Pretend the remote has many extra bookmarks (feature/foo,
        // release/1.0, gh-readonly-queue/main/...). The test enforces
        // its expectation negatively: only `main@origin` and
        // `master@origin` may be tracked. Any other `bookmark track`
        // call would crash FakeRunner with "unexpected command".
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "--colocate",
                    "git@github.com:spinyfin/mono.git",
                    &staging.display().to_string(),
                ],
                "",
            )
            .creating_dir(staging.clone()),
            ExpectedCommand::ok(
                staging.clone(),
                "jj",
                &["bookmark", "track", "main@origin"],
                "",
            ),
            ExpectedCommand::no_such_remote_bookmark(
                staging.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
            ),
            ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                new_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "ba5eba11",
            ),
        ]);

        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "no extra bookmark tracking",
        ]);
        run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");
        runner.assert_exhausted();
    }

    /// If a freshly-cloned repo has neither `main@origin` nor
    /// `master@origin`, cube cannot promote any default branch to
    /// local tracking. Lease must hard-fail with a setup-step error
    /// rather than silently proceeding into `jj new <missing>` later
    /// (which would surface as a confusing unrelated jj error).
    #[test]
    fn workspace_lease_errors_when_no_default_origin_bookmark_exists() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "weird",
            "--origin",
            "git@github.com:spinyfin/weird.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "weird-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let staging = workspace_root.join(".incoming-weird-agent-001");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "--colocate",
                    "git@github.com:spinyfin/weird.git",
                    &staging.display().to_string(),
                ],
                "",
            )
            .creating_dir(staging.clone()),
            ExpectedCommand::no_such_remote_bookmark(
                staging.clone(),
                "jj",
                &["bookmark", "track", "main@origin"],
            ),
            ExpectedCommand::no_such_remote_bookmark(
                staging.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
            ),
        ]);

        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "weird",
            "--task",
            "no default branch",
        ]);
        let err = run_with_dependencies(lease, Some(&database_path), &runner)
            .expect_err("lease should fail when neither default branch is present");
        match err {
            CubeError::SetupStepFailed { step, error } => {
                assert_eq!(step, "track_remote_bookmarks");
                assert!(
                    error.contains("main@origin") && error.contains("master@origin"),
                    "error message should name both expected branches: {error}"
                );
            }
            other => panic!("expected SetupStepFailed, got {other:?}"),
        }
        runner.assert_exhausted();
    }

    /// If `jj bookmark track main@origin` fails with anything other
    /// than "no such remote bookmark" (e.g. jj is broken, network
    /// failure mid-clone), cube must propagate the error rather than
    /// swallowing it. Pins the precision of the error-tolerance
    /// classifier: only the bookmark-doesn't-exist case is benign.
    #[test]
    fn workspace_lease_propagates_unrelated_track_failure() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let staging = workspace_root.join(".incoming-mono-agent-001");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "--colocate",
                    "git@github.com:spinyfin/mono.git",
                    &staging.display().to_string(),
                ],
                "",
            )
            .creating_dir(staging.clone()),
            ExpectedCommand {
                cwd: staging.clone(),
                program: "jj".to_string(),
                args: vec![
                    "bookmark".to_string(),
                    "track".to_string(),
                    "main@origin".to_string(),
                ],
                result: Err(CubeError::CommandFailed {
                    program: "jj".to_string(),
                    args: vec![
                        "bookmark".to_string(),
                        "track".to_string(),
                        "main@origin".to_string(),
                    ],
                    status: Some(2),
                    stderr: "Error: Failed to load repo: some unrelated jj failure".to_string(),
                }),
                creates_dir: None,
            },
        ]);

        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "track failure propagates",
        ]);
        let err = run_with_dependencies(lease, Some(&database_path), &runner)
            .expect_err("lease should propagate non-NoSuchRemoteBookmark failures");
        match err {
            CubeError::CommandFailed { program, stderr, .. } => {
                assert_eq!(program, "jj");
                assert!(stderr.contains("unrelated jj failure"), "stderr={stderr}");
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }
        runner.assert_exhausted();
    }

    #[test]
    fn workspace_lease_auto_creates_next_id_after_existing() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("workspace dir");
        std::fs::create_dir_all(workspace_root.join("mono-agent-007").join(".jj")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        // Lease both existing workspaces first so the pool is exhausted
        for (path, task) in [
            (workspace_root.join("mono-agent-001"), "first"),
            (workspace_root.join("mono-agent-007"), "second"),
        ] {
            let runner = FakeRunner::new(vec![
                ExpectedCommand::ok(path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
                ExpectedCommand::ok(path.clone(), "jj", &["git", "fetch"], ""),
                ExpectedCommand::ok(path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
                ExpectedCommand::ok(path.clone(), "jj", &["new", "main"], ""),
                ExpectedCommand::ok(
                    path.clone(),
                    "jj",
                    &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                    "deadbee",
                ),
            ]);
            let lease = Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", task]);
            run_with_dependencies(lease, Some(&database_path), &runner).expect("seed lease");
        }

        // Pool now exhausted; next lease should clone mono-agent-008 (max+1)
        let new_path = workspace_root.join("mono-agent-008");
        let staging = workspace_root.join(".incoming-mono-agent-008");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "--colocate",
                    "git@github.com:spinyfin/mono.git",
                    &staging.display().to_string(),
                ],
                "",
            )
            .creating_dir(staging.clone()),
            ExpectedCommand::ok(
                staging.clone(),
                "jj",
                &["bookmark", "track", "main@origin"],
                "",
            ),
            ExpectedCommand::no_such_remote_bookmark(
                staging.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
            ),
            ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                new_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        let lease = Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "third"]);
        let result = run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

        assert_eq!(
            result.payload["workspace"]["workspace_id"],
            "mono-agent-008"
        );
        runner.assert_exhausted();
    }

    #[test]
    fn next_workspace_id_picks_max_plus_one() {
        assert_eq!(
            super::next_workspace_id("mono-agent-", &[]),
            "mono-agent-001"
        );
        assert_eq!(
            super::next_workspace_id(
                "mono-agent-",
                &["mono-agent-001".to_string(), "mono-agent-002".to_string(),],
            ),
            "mono-agent-003"
        );
        // Non-contiguous: jumps to max+1, doesn't fill the gap.
        assert_eq!(
            super::next_workspace_id(
                "mono-agent-",
                &["mono-agent-001".to_string(), "mono-agent-007".to_string(),],
            ),
            "mono-agent-008"
        );
        // Mixed-prefix or non-numeric IDs are ignored.
        assert_eq!(
            super::next_workspace_id(
                "mono-agent-",
                &[
                    "flunge-agent-099".to_string(),
                    "mono-agent-abc".to_string(),
                    "mono-agent-002".to_string(),
                ],
            ),
            "mono-agent-003"
        );
    }

    #[test]
    fn workspace_lease_with_prefer_claims_named_workspace_when_free() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");
        std::fs::create_dir_all(workspace_root.join("mono-agent-005").join(".jj")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let preferred_path = workspace_root.join("mono-agent-005");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(preferred_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(preferred_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(preferred_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(preferred_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                preferred_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "def5678",
            ),
        ]);

        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "resume cube prefer work",
            "--prefer",
            "mono-agent-005",
        ]);
        let result = run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

        assert_eq!(
            result.payload["workspace"]["workspace_id"],
            "mono-agent-005"
        );
        assert_eq!(
            result.payload["workspace"]["workspace_path"],
            preferred_path.display().to_string()
        );
        runner.assert_exhausted();
    }

    #[test]
    fn workspace_lease_with_prefer_falls_back_when_preferred_is_leased() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");
        std::fs::create_dir_all(workspace_root.join("mono-agent-005").join(".jj")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        // First lease takes mono-agent-005 (the preferred one).
        let preferred_path = workspace_root.join("mono-agent-005");
        let first_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(preferred_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(preferred_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(preferred_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(preferred_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                preferred_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "first123",
            ),
        ]);
        let first_lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "first task",
            "--prefer",
            "mono-agent-005",
        ]);
        run_with_dependencies(first_lease, Some(&database_path), &first_runner)
            .expect("first lease");
        first_runner.assert_exhausted();

        // Second lease prefers mono-agent-005 (leased), should fall back to mono-agent-004.
        let fallback_path = workspace_root.join("mono-agent-004");
        let second_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(fallback_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(fallback_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(fallback_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(fallback_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                fallback_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "second456",
            ),
        ]);
        let second_lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "second task",
            "--prefer",
            "mono-agent-005",
        ]);
        let result = run_with_dependencies(second_lease, Some(&database_path), &second_runner)
            .expect("second lease");

        assert_eq!(
            result.payload["workspace"]["workspace_id"],
            "mono-agent-004"
        );
        second_runner.assert_exhausted();
    }

    #[test]
    fn workspace_lease_with_unknown_prefer_falls_back_to_first_free() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");
        std::fs::create_dir_all(workspace_root.join("mono-agent-005").join(".jj")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let first_path = workspace_root.join("mono-agent-004");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(first_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(first_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(first_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(first_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                first_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);

        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "fallback path",
            "--prefer",
            "mono-agent-999",
        ]);
        let result = run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

        assert_eq!(
            result.payload["workspace"]["workspace_id"],
            "mono-agent-004"
        );
        runner.assert_exhausted();
    }

    // ── Health-check tests ───────────────────────────────────────────────

    fn jj_status_clean() -> &'static str {
        "The working copy is clean\nWorking copy : abc1234 (empty) main"
    }

    fn jj_status_dirty() -> &'static str {
        "Working copy changes:\nM tools/cube/src/app.rs\n\nWorking copy : abc1234 some change"
    }

    fn jj_status_conflicted(bookmark: &str) -> String {
        format!(
            "The working copy is clean\nWorking copy : abc1234 (empty) main\nThese bookmarks have conflicts:\n  {bookmark}\n  Use `jj bookmark list` to see details."
        )
    }

    #[test]
    fn workspace_lease_clean_pool_returns_lowest_workspace() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-003").join(".jj")).expect("workspace dir");
        std::fs::create_dir_all(workspace_root.join("mono-agent-007").join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let first = workspace_root.join("mono-agent-003");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(first.clone(), "jj", &["status", "--no-pager"], jj_status_clean()),
            ExpectedCommand::ok(first.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(first.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(first.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                first.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "clean pool"]),
            Some(&database_path),
            &runner,
        )
        .expect("lease");
        runner.assert_exhausted();

        assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-003");
        // health_check array should be present
        let hc = result.payload["health_check"].as_array().expect("health_check");
        assert_eq!(hc.len(), 1);
        assert_eq!(hc[0]["workspace_id"], "mono-agent-003");
        assert_eq!(hc[0]["health"], "clean");
        assert_eq!(hc[0]["skipped"], false);
    }

    #[test]
    fn workspace_lease_skips_dirty_picks_clean() {
        // Pool: dirty(003), clean(007) → should skip 003, lease 007.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let dirty_path = workspace_root.join("mono-agent-003");
        let clean_path = workspace_root.join("mono-agent-007");
        std::fs::create_dir_all(dirty_path.join(".jj")).expect("dirty dir");
        std::fs::create_dir_all(clean_path.join(".jj")).expect("clean dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let runner = FakeRunner::new(vec![
            // health-check 003 → dirty → skip
            ExpectedCommand::ok(dirty_path.clone(), "jj", &["status", "--no-pager"], jj_status_dirty()),
            // health-check 007 → clean → use
            ExpectedCommand::ok(clean_path.clone(), "jj", &["status", "--no-pager"], jj_status_clean()),
            ExpectedCommand::ok(clean_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(clean_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(clean_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                clean_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "skip dirty"]),
            Some(&database_path),
            &runner,
        )
        .expect("lease");
        runner.assert_exhausted();

        assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-007");
        let hc = result.payload["health_check"].as_array().expect("health_check");
        assert_eq!(hc.len(), 2);
        assert_eq!(hc[0]["workspace_id"], "mono-agent-003");
        assert_eq!(hc[0]["health"], "dirty");
        assert_eq!(hc[0]["skipped"], true);
        assert_eq!(hc[1]["workspace_id"], "mono-agent-007");
        assert_eq!(hc[1]["health"], "clean");
        assert_eq!(hc[1]["skipped"], false);

        // mono-agent-003 must be marked dirty in the store
        use crate::store::Store;
        let store = Store::open_at(&database_path).unwrap();
        let ws = store
            .get_workspace_by_path(&dirty_path)
            .unwrap()
            .unwrap();
        assert_eq!(ws.health_status, Some(crate::metadata::WorkspaceHealth::Dirty));
    }

    #[test]
    fn workspace_lease_allow_dirty_reclaims_named_workspace_without_reset() {
        // --allow-dirty must claim the preferred workspace as-is and run
        // NO health check, NO `jj git fetch`, and NO `jj new main` — the
        // dirty tree is handed to the new lease-holder intact. The only jj
        // call is the head-commit read.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj"))
            .expect("workspace dir");
        let dirty_path = workspace_root.join("mono-agent-005");
        std::fs::create_dir_all(dirty_path.join(".jj")).expect("dirty dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // Seed the registry rows and mark mono-agent-005 dirty, mimicking a
        // crashed worker whose unpushed work was left behind. The normal
        // lease path would skip this workspace; --allow-dirty must not.
        {
            use crate::metadata::{WorkspaceCandidate, WorkspaceHealth};
            use crate::store::Store;
            let mut store = Store::open_at(&database_path).unwrap();
            store
                .sync_workspaces(
                    "mono",
                    &[
                        WorkspaceCandidate {
                            workspace_id: "mono-agent-004".to_string(),
                            workspace_path: workspace_root.join("mono-agent-004"),
                        },
                        WorkspaceCandidate {
                            workspace_id: "mono-agent-005".to_string(),
                            workspace_path: dirty_path.clone(),
                        },
                    ],
                )
                .unwrap();
            store
                .update_workspace_health("mono", "mono-agent-005", WorkspaceHealth::Dirty)
                .unwrap();
        }

        let runner = FakeRunner::new(vec![ExpectedCommand::ok(
            dirty_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "dead789",
        )]);

        let result = run_with_dependencies(
            Cli::parse_from([
                "cube",
                "workspace",
                "lease",
                "mono",
                "--task",
                "recover stranded work",
                "--prefer",
                "mono-agent-005",
                "--allow-dirty",
            ]),
            Some(&database_path),
            &runner,
        )
        .expect("lease");
        // assert_exhausted proves no fetch/new-main/status ran — reset skipped.
        runner.assert_exhausted();

        assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-005");
        assert_eq!(
            result.payload["workspace"]["workspace_path"],
            dirty_path.display().to_string()
        );
        assert_eq!(result.payload["workspace"]["head_commit"], "dead789");
        let hc = result.payload["health_check"].as_array().expect("health_check");
        assert_eq!(hc.len(), 1);
        assert_eq!(hc[0]["allow_dirty"], true);
        assert_eq!(hc[0]["reset_skipped"], true);

        // The row is now leased to this holder.
        use crate::store::Store;
        let store = Store::open_at(&database_path).unwrap();
        let ws = store.get_workspace_by_path(&dirty_path).unwrap().unwrap();
        assert_eq!(ws.state, crate::metadata::WorkspaceState::Leased);
    }

    #[test]
    fn workspace_lease_allow_dirty_errors_when_preferred_missing() {
        // --allow-dirty must never silently fall back to a fresh
        // workspace: if the named workspace is unknown, fail loudly so the
        // recovering worker is not routed away from the dirty tree.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj"))
            .expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let runner = FakeRunner::new(vec![]);
        let err = run_with_dependencies(
            Cli::parse_from([
                "cube",
                "workspace",
                "lease",
                "mono",
                "--task",
                "recover stranded work",
                "--prefer",
                "mono-agent-999",
                "--allow-dirty",
            ]),
            Some(&database_path),
            &runner,
        )
        .expect_err("expected lease to fail for unknown preferred workspace");
        runner.assert_exhausted();
        assert!(matches!(err, CubeError::WorkspaceNotFound(_)));
    }

    #[test]
    fn workspace_lease_allow_dirty_errors_when_preferred_leased() {
        // A live lease on the preferred workspace must block dirty reclaim
        // rather than stomping the active holder's working copy.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let busy_path = workspace_root.join("mono-agent-004");
        std::fs::create_dir_all(busy_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // First lease takes mono-agent-004.
        let first_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(busy_path.clone(), "jj", &["status", "--no-pager"], jj_status_clean()),
            ExpectedCommand::ok(busy_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(busy_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(busy_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                busy_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "live0001",
            ),
        ]);
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "live work"]),
            Some(&database_path),
            &first_runner,
        )
        .expect("first lease");
        first_runner.assert_exhausted();

        let runner = FakeRunner::new(vec![]);
        let err = run_with_dependencies(
            Cli::parse_from([
                "cube",
                "workspace",
                "lease",
                "mono",
                "--task",
                "recover stranded work",
                "--prefer",
                "mono-agent-004",
                "--allow-dirty",
            ]),
            Some(&database_path),
            &runner,
        )
        .expect_err("expected lease to fail for leased preferred workspace");
        runner.assert_exhausted();
        assert!(matches!(err, CubeError::InvalidArgument(_)));
    }

    // ── --resume_pr tests ──────────────────────────────────────────────────────

    /// Helper: build the expected command sequence for a `--resume_pr` lease.
    ///
    /// The sequence is the same for both the warm path (local `pr/<n>` bookmark
    /// already present) and the cold path (bookmark absent — both call `gh pr
    /// view` and then `jj bookmark set`).
    fn resume_pr_runner_for(
        workspace_path: &std::path::Path,
        pr_number: u64,
        head_branch: &str,
        head_commit: &str,
    ) -> FakeRunner {
        let github_remote = "github";
        let remote_list = format!("origin\t/local/mirror\n{github_remote}\tgit@github.com:spinyfin/mono.git\n");
        let pr_json = format!(
            r#"{{"headRefName":"{head_branch}","headRefOid":"deadbeef1234567890","state":"OPEN"}}"#
        );
        let remote_ref = format!("{head_branch}@{github_remote}");
        let pr_bm = format!("pr/{pr_number}");
        FakeRunner::new(vec![
            // Health check
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            // Resolve github remote
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["git", "remote", "list"], &remote_list),
            // Fetch from GitHub remote (--remote <github_remote>)
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["git", "fetch", "--remote", github_remote], ""),
            // Resolve PR head from gh
            ExpectedCommand::ok(workspace_path.to_path_buf(), "gh", &[
                "pr", "view", &pr_number.to_string(),
                "-R", "spinyfin/mono",
                "--json", "headRefName,headRefOid,state",
            ], &pr_json),
            // Set pr/<n> bookmark
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["bookmark", "set", &pr_bm, "-r", &remote_ref], ""),
            // Set head-branch bookmark
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["bookmark", "set", head_branch, "-r", &remote_ref, "--allow-backwards"], ""),
            // Land on PR head
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["new", &pr_bm], ""),
            // Record head_commit
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"], head_commit),
        ])
    }

    #[test]
    fn workspace_lease_resume_pr_warm_path_positions_on_pr_head() {
        // "Warm path": the workspace was previously used for PR 1364, so the
        // local `pr/1364` bookmark is already present. The resume sequence runs
        // the same commands regardless (gh always consulted for reconciliation).
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj"))
            .expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let ws_path = workspace_root.join("mono-agent-004");
        let runner = resume_pr_runner_for(&ws_path, 1364, "boss/exec_18b6_a1", "cafe1234");

        let result = run_with_dependencies(
            Cli::parse_from([
                "cube", "workspace", "lease", "mono",
                "--task", "resume PR 1364",
                "--resume-pr", "1364",
            ]),
            Some(&database_path),
            &runner,
        )
        .expect("resume_pr lease");
        runner.assert_exhausted();

        assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-004");
        assert_eq!(result.payload["workspace"]["head_commit"], "cafe1234");
        assert_eq!(result.payload["resume_pr"]["pr_number"], 1364);
        assert_eq!(result.payload["resume_pr"]["head_branch"], "boss/exec_18b6_a1");
    }

    #[test]
    fn workspace_lease_resume_pr_cold_path_fallback_via_gh() {
        // "Cold path": the workspace has never seen PR 42 before; the local
        // `pr/42` bookmark is absent. The resume sequence still calls `gh pr
        // view` and creates the bookmark on the fetched ref.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj"))
            .expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let ws_path = workspace_root.join("mono-agent-004");
        let runner = resume_pr_runner_for(&ws_path, 42, "boss/exec_cold_b2", "deadc0de");

        let result = run_with_dependencies(
            Cli::parse_from([
                "cube", "workspace", "lease", "mono",
                "--task", "resume cold PR 42",
                "--resume-pr", "42",
            ]),
            Some(&database_path),
            &runner,
        )
        .expect("cold resume_pr lease");
        runner.assert_exhausted();

        assert_eq!(result.payload["resume_pr"]["pr_number"], 42);
        assert_eq!(result.payload["resume_pr"]["head_branch"], "boss/exec_cold_b2");
        assert_eq!(result.payload["workspace"]["head_commit"], "deadc0de");
    }

    #[test]
    fn workspace_lease_resume_pr_with_prefer_uses_preferred_workspace() {
        // --prefer + --resume_pr: the preferred workspace is free and gets
        // leased, then positioned on the PR head.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj"))
            .expect("ws-004 dir");
        std::fs::create_dir_all(workspace_root.join("mono-agent-007").join(".jj"))
            .expect("ws-007 dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // --prefer mono-agent-007 → that workspace must be health-checked first
        // and then positioned on the PR head.
        let preferred_path = workspace_root.join("mono-agent-007");
        let runner = resume_pr_runner_for(&preferred_path, 99, "boss/exec_pref_c3", "feedface");

        let result = run_with_dependencies(
            Cli::parse_from([
                "cube", "workspace", "lease", "mono",
                "--task", "resume PR 99 on preferred workspace",
                "--prefer", "mono-agent-007",
                "--resume-pr", "99",
            ]),
            Some(&database_path),
            &runner,
        )
        .expect("prefer + resume_pr lease");
        runner.assert_exhausted();

        assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-007");
        assert_eq!(result.payload["resume_pr"]["pr_number"], 99);
        assert_eq!(result.payload["resume_pr"]["head_branch"], "boss/exec_pref_c3");
    }

    #[test]
    fn workspace_lease_resume_pr_fallback_when_prefer_absent() {
        // --prefer names a workspace that doesn't exist → cube silently falls
        // back to another free workspace and still positions on the PR head.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        // Only mono-agent-004 exists; the preferred mono-agent-999 does not.
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj"))
            .expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let ws_path = workspace_root.join("mono-agent-004");
        let runner = resume_pr_runner_for(&ws_path, 77, "boss/exec_fallback_d4", "b0b0b0b0");

        let result = run_with_dependencies(
            Cli::parse_from([
                "cube", "workspace", "lease", "mono",
                "--task", "resume with fallback",
                "--prefer", "mono-agent-999",
                "--resume-pr", "77",
            ]),
            Some(&database_path),
            &runner,
        )
        .expect("fallback resume_pr lease");
        runner.assert_exhausted();

        assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-004");
        assert_eq!(result.payload["resume_pr"]["pr_number"], 77);
    }

    #[test]
    fn workspace_lease_resume_pr_hard_errors_on_merged_pr() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj"))
            .expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let ws_path = workspace_root.join("mono-agent-004");
        let pr_json = r#"{"headRefName":"boss/exec_merged","headRefOid":"deadbeef","state":"MERGED"}"#;
        let remote_list = "origin\t/local/mirror\ngithub\tgit@github.com:spinyfin/mono.git\n";
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(ws_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(ws_path.clone(), "jj", &["git", "remote", "list"], remote_list),
            ExpectedCommand::ok(ws_path.clone(), "jj", &["git", "fetch", "--remote", "github"], ""),
            ExpectedCommand::ok(ws_path.clone(), "gh", &[
                "pr", "view", "5",
                "-R", "spinyfin/mono",
                "--json", "headRefName,headRefOid,state",
            ], pr_json),
        ]);

        let err = run_with_dependencies(
            Cli::parse_from([
                "cube", "workspace", "lease", "mono",
                "--task", "attempt to resume merged PR",
                "--resume-pr", "5",
            ]),
            Some(&database_path),
            &runner,
        )
        .expect_err("expected hard error for merged PR");
        runner.assert_exhausted();

        let msg = err.to_string();
        assert!(msg.contains("MERGED"), "error should mention MERGED: {msg}");
        assert!(msg.contains("5"), "error should mention PR number: {msg}");
    }

    #[test]
    fn workspace_lease_resume_pr_hard_errors_on_closed_pr() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj"))
            .expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let ws_path = workspace_root.join("mono-agent-004");
        let pr_json = r#"{"headRefName":"boss/exec_closed","headRefOid":"cafebabe","state":"CLOSED"}"#;
        let remote_list = "origin\t/local/mirror\ngithub\tgit@github.com:spinyfin/mono.git\n";
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(ws_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(ws_path.clone(), "jj", &["git", "remote", "list"], remote_list),
            ExpectedCommand::ok(ws_path.clone(), "jj", &["git", "fetch", "--remote", "github"], ""),
            ExpectedCommand::ok(ws_path.clone(), "gh", &[
                "pr", "view", "10",
                "-R", "spinyfin/mono",
                "--json", "headRefName,headRefOid,state",
            ], pr_json),
        ]);

        let err = run_with_dependencies(
            Cli::parse_from([
                "cube", "workspace", "lease", "mono",
                "--task", "attempt to resume closed PR",
                "--resume-pr", "10",
            ]),
            Some(&database_path),
            &runner,
        )
        .expect_err("expected hard error for closed PR");
        runner.assert_exhausted();

        let msg = err.to_string();
        assert!(msg.contains("CLOSED"), "error should mention CLOSED: {msg}");
    }

    #[test]
    fn workspace_lease_resume_pr_json_omits_resume_pr_when_not_used() {
        // Normal lease without --resume_pr must not include "resume_pr" in JSON.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj"))
            .expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let ws_path = workspace_root.join("mono-agent-004");
        let runner = lease_runner_for(&ws_path, "abc1234");
        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "normal task"]),
            Some(&database_path),
            &runner,
        )
        .expect("normal lease");
        runner.assert_exhausted();

        assert!(
            result.payload.get("resume_pr").is_none(),
            "normal lease must not include resume_pr in JSON: {:?}",
            result.payload
        );
    }

    #[test]
    fn workspace_lease_one_clean_n_conflicted_uses_clean() {
        // Pool: conflicted(003), clean(007) → should skip conflicted, use clean.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let conflicted_path = workspace_root.join("mono-agent-003");
        let clean_path = workspace_root.join("mono-agent-007");
        std::fs::create_dir_all(conflicted_path.join(".jj")).expect("conflicted dir");
        std::fs::create_dir_all(clean_path.join(".jj")).expect("clean dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let runner = FakeRunner::new(vec![
            // health-check 003 → conflicted (save as fallback, keep looking)
            ExpectedCommand::ok(
                conflicted_path.clone(),
                "jj",
                &["status", "--no-pager"],
                &jj_status_conflicted("fix-burst"),
            ),
            // health-check 007 → clean → use
            ExpectedCommand::ok(clean_path.clone(), "jj", &["status", "--no-pager"], jj_status_clean()),
            ExpectedCommand::ok(clean_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(clean_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(clean_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                clean_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "prefer clean"]),
            Some(&database_path),
            &runner,
        )
        .expect("lease");
        runner.assert_exhausted();

        // 007 was used; 003 (conflicted) was not repaired because 007 was clean.
        assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-007");
        let hc = result.payload["health_check"].as_array().expect("health_check");
        assert_eq!(hc.len(), 2);
        assert_eq!(hc[0]["workspace_id"], "mono-agent-003");
        assert_eq!(hc[0]["health"], "conflicted");
        assert_eq!(hc[1]["workspace_id"], "mono-agent-007");
        assert_eq!(hc[1]["health"], "clean");
    }

    #[test]
    fn workspace_lease_all_conflicted_repairs_lowest_and_returns_it() {
        // Pool: conflicted(003), conflicted(007) → repair 003 (lowest) and use it.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let path_003 = workspace_root.join("mono-agent-003");
        let path_007 = workspace_root.join("mono-agent-007");
        std::fs::create_dir_all(path_003.join(".jj")).expect("003 dir");
        std::fs::create_dir_all(path_007.join(".jj")).expect("007 dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let runner = FakeRunner::new(vec![
            // health-check 003 → conflicted (save as first fallback)
            ExpectedCommand::ok(
                path_003.clone(),
                "jj",
                &["status", "--no-pager"],
                &jj_status_conflicted("fix-burst"),
            ),
            // health-check 007 → conflicted (already have a fallback, don't replace)
            ExpectedCommand::ok(
                path_007.clone(),
                "jj",
                &["status", "--no-pager"],
                &jj_status_conflicted("fix-burst"),
            ),
            // repair 003: forget the conflicted bookmark
            ExpectedCommand::ok(
                path_003.clone(),
                "jj",
                &["bookmark", "forget", "fix-burst"],
                "",
            ),
            // reset 003
            ExpectedCommand::ok(path_003.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(path_003.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(path_003.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                path_003.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "all conflicted"]),
            Some(&database_path),
            &runner,
        )
        .expect("lease");
        runner.assert_exhausted();

        assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-003");
        let hc = result.payload["health_check"].as_array().expect("health_check");
        // Both conflicted workspaces appear in health_check
        assert_eq!(hc.len(), 2);
        assert_eq!(hc[0]["workspace_id"], "mono-agent-003");
        assert_eq!(hc[0]["health"], "conflicted");
        // 003 was chosen (not skipped), 007 was skipped (already have a candidate)
        assert_eq!(hc[0]["skipped"], false);
        assert_eq!(hc[1]["workspace_id"], "mono-agent-007");
        assert_eq!(hc[1]["skipped"], true);
    }

    #[test]
    fn workspace_lease_all_dirty_auto_creates_fresh_workspace() {
        // Pool: dirty(003), dirty(007) → no reusable slot → auto-create a new
        // workspace instead of blocking. The dirty entries must be preserved.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let path_003 = workspace_root.join("mono-agent-003");
        let path_007 = workspace_root.join("mono-agent-007");
        std::fs::create_dir_all(path_003.join(".jj")).expect("003 dir");
        std::fs::create_dir_all(path_007.join(".jj")).expect("007 dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // After health-checking 003 and 007 as dirty, the lease path falls
        // through to auto_create_workspace which clones a new workspace.
        let new_path = workspace_root.join("mono-agent-008");
        let staging = workspace_root.join(".incoming-mono-agent-008");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(path_003.clone(), "jj", &["status", "--no-pager"], jj_status_dirty()),
            ExpectedCommand::ok(path_007.clone(), "jj", &["status", "--no-pager"], jj_status_dirty()),
            ExpectedCommand::ok(
                workspace_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "--colocate",
                    "git@github.com:spinyfin/mono.git",
                    &staging.display().to_string(),
                ],
                "",
            )
            .creating_dir(staging.clone()),
            ExpectedCommand::ok(staging.clone(), "jj", &["bookmark", "track", "main@origin"], ""),
            ExpectedCommand::no_such_remote_bookmark(
                staging.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
            ),
            ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                new_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "all dirty"]),
            Some(&database_path),
            &runner,
        )
        .expect("lease should succeed via auto-create when all existing workspaces are dirty");
        runner.assert_exhausted();

        // The leased workspace is the newly created one.
        assert_eq!(
            result.payload["workspace"]["workspace_id"],
            "mono-agent-008",
            "expected newly created workspace"
        );
        assert_eq!(result.payload["workspace"]["state"], "leased");

        // Both dirty workspaces are still in the store with their health marked.
        use crate::store::Store;
        let store = Store::open_at(&database_path).unwrap();
        for path in [&path_003, &path_007] {
            let ws = store.get_workspace_by_path(path).unwrap().unwrap();
            assert_eq!(
                ws.health_status,
                Some(crate::metadata::WorkspaceHealth::Dirty),
                "dirty workspace should be preserved at {}",
                path.display()
            );
        }
    }

    #[test]
    fn workspace_list_shows_health_status_in_effective_state() {
        // After a lease attempt that skips dirty workspaces, workspace list
        // should show `free-dirty` for the skipped ones.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let dirty_path = workspace_root.join("mono-agent-003");
        let clean_path = workspace_root.join("mono-agent-007");
        std::fs::create_dir_all(dirty_path.join(".jj")).expect("dirty dir");
        std::fs::create_dir_all(clean_path.join(".jj")).expect("clean dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // Trigger a lease so health checks run and health_status is persisted.
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(dirty_path.clone(), "jj", &["status", "--no-pager"], jj_status_dirty()),
            ExpectedCommand::ok(clean_path.clone(), "jj", &["status", "--no-pager"], jj_status_clean()),
            ExpectedCommand::ok(clean_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(clean_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(clean_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                clean_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "test"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        lease_runner.assert_exhausted();

        // Now list workspaces and check the JSON output.
        let list_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "list", "--json"]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("list");

        let workspaces = list_result.payload["workspaces"]
            .as_array()
            .expect("workspaces array");
        // 003 is free-dirty, 007 is leased
        let ws_003 = workspaces
            .iter()
            .find(|w| w["workspace_id"] == "mono-agent-003")
            .expect("003");
        let ws_007 = workspaces
            .iter()
            .find(|w| w["workspace_id"] == "mono-agent-007")
            .expect("007");
        assert_eq!(ws_003["health_status"], "dirty");
        assert_eq!(ws_003["state"], "free");
        assert_eq!(ws_007["state"], "leased");
    }

    #[test]
    fn workspace_list_state_filter_accepts_free_dirty() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let dirty_path = workspace_root.join("mono-agent-003");
        let clean_path = workspace_root.join("mono-agent-007");
        std::fs::create_dir_all(dirty_path.join(".jj")).expect("dirty dir");
        std::fs::create_dir_all(clean_path.join(".jj")).expect("clean dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // Trigger a lease to run health checks and persist health_status.
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(dirty_path.clone(), "jj", &["status", "--no-pager"], jj_status_dirty()),
            ExpectedCommand::ok(clean_path.clone(), "jj", &["status", "--no-pager"], jj_status_clean()),
            ExpectedCommand::ok(clean_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(clean_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(clean_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                clean_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "test"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");

        // --state free-dirty should return only mono-agent-003
        let dirty_list = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "list", "--state", "free-dirty"]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("list dirty");

        let workspaces = dirty_list.payload["workspaces"]
            .as_array()
            .expect("workspaces");
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0]["workspace_id"], "mono-agent-003");

        // --state free should return zero (003 is free-dirty, 007 is leased)
        let free_list = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "list", "--state", "free"]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("list free");
        assert_eq!(
            free_list.payload["workspaces"].as_array().unwrap().len(),
            0,
            "no purely-free workspaces should remain after leasing the only clean one"
        );
    }

    #[test]
    fn workspace_release_clears_health_status() {
        // After a workspace is released, its health_status should be NULL
        // so it gets re-checked at next lease time.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let ws_path = workspace_root.join("mono-agent-003");
        std::fs::create_dir_all(ws_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let lease_runner = lease_runner_for(&ws_path, "abc1234");
        let lease_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "test"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        lease_runner.assert_exhausted();
        let lease_id = lease_result.payload["workspace"]["lease_id"]
            .as_str()
            .unwrap()
            .to_string();

        let release_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(ws_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(ws_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(ws_path.clone(), "jj", &["new", "main"], ""),
            gc_noop_command(&ws_path),
            gc_pr_remote_noop_command(&ws_path),
        ]);
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
            Some(&database_path),
            &release_runner,
        )
        .expect("release");
        release_runner.assert_exhausted();

        use crate::store::Store;
        let store = Store::open_at(&database_path).unwrap();
        let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
        assert_eq!(ws.health_status, None, "health_status should be cleared on release");
    }

    #[test]
    fn workspace_lease_release_list_workspace_list_shows_effective_state() {
        // `cube workspace list` output message should show `free-conflicted`
        // for a workspace whose health_status is `conflicted`.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let conflicted_path = workspace_root.join("mono-agent-003");
        let clean_path = workspace_root.join("mono-agent-007");
        std::fs::create_dir_all(conflicted_path.join(".jj")).expect("conflicted dir");
        std::fs::create_dir_all(clean_path.join(".jj")).expect("clean dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // Run a lease that skips the conflicted workspace and picks the clean one.
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                conflicted_path.clone(),
                "jj",
                &["status", "--no-pager"],
                &jj_status_conflicted("fix-burst"),
            ),
            ExpectedCommand::ok(clean_path.clone(), "jj", &["status", "--no-pager"], jj_status_clean()),
            ExpectedCommand::ok(clean_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(clean_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(clean_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                clean_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "test"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        lease_runner.assert_exhausted();

        let list = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "list"]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("list");

        // The human-readable message should contain "free-conflicted" for 003.
        assert!(
            list.message.contains("free-conflicted"),
            "expected free-conflicted in list message: {}",
            list.message
        );
    }

    #[test]
    fn workspace_release_resets_and_frees_the_workspace() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let workspace_path = workspace_root.join("mono-agent-004");
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "implement cube",
        ]);
        let lease_result =
            run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
        let lease_id = lease_result.payload["workspace"]["lease_id"]
            .as_str()
            .expect("lease id")
            .to_string();
        lease_runner.assert_exhausted();

        let release_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            gc_noop_command(&workspace_path),
            gc_pr_remote_noop_command(&workspace_path),
        ]);
        let release = Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]);
        let release_result =
            run_with_dependencies(release, Some(&database_path), &release_runner).expect("release");

        assert_eq!(release_result.payload["workspace"]["state"], "free");
        assert_eq!(
            release_result.payload["workspace"]["lease_id"],
            serde_json::Value::Null
        );
        release_runner.assert_exhausted();
    }

    #[test]
    fn lease_and_release_emit_audit_log_entries() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let workspace_path = workspace_root.join("mono-agent-004");
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "audit smoke",
        ]);
        let lease_result =
            run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
        let lease_id = lease_result.payload["workspace"]["lease_id"]
            .as_str()
            .expect("lease id")
            .to_string();
        lease_runner.assert_exhausted();

        let release_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            gc_noop_command(&workspace_path),
            gc_pr_remote_noop_command(&workspace_path),
        ]);
        let release = Cli::parse_from([
            "cube",
            "workspace",
            "release",
            "--lease",
            &lease_id,
            "--reason",
            "done",
        ]);
        run_with_dependencies(release, Some(&database_path), &release_runner).expect("release");
        release_runner.assert_exhausted();

        let audit_dir = tempdir.path().join("audit");
        let audit_files: Vec<_> = std::fs::read_dir(&audit_dir)
            .expect("audit dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        assert_eq!(audit_files.len(), 1, "expected one weekly audit file");

        let contents = std::fs::read_to_string(&audit_files[0]).expect("audit content");
        let events: Vec<serde_json::Value> = contents
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .collect();
        let by_event: Vec<&serde_json::Value> = events
            .iter()
            .filter(|e| {
                let name = e["event"].as_str().unwrap_or_default();
                name == "lease.acquired" || name == "lease.released"
            })
            .collect();
        assert_eq!(by_event.len(), 2, "expected one lease.acquired + one lease.released event");

        let acquired = by_event[0];
        assert_eq!(acquired["event"], "lease.acquired");
        assert_eq!(acquired["repo"], "mono");
        assert_eq!(acquired["workspace_id"], "mono-agent-004");
        assert_eq!(acquired["lease_id"], lease_id);
        assert_eq!(acquired["task"], "audit smoke");
        assert_eq!(acquired["head_commit"], "abc1234");
        assert!(acquired["holder"].is_string());
        assert!(acquired["ts"].as_str().unwrap().ends_with('Z'));

        let released = by_event[1];
        assert_eq!(released["event"], "lease.released");
        assert_eq!(released["lease_id"], lease_id);
        assert_eq!(released["reason"], "done");
        assert_eq!(released["keep_dirty"], false);

        // The instrumentation chore also requires that every `jj`
        // operation cube runs against a leased workspace is auditable.
        // Each reset emits a fetch + bookmark-set + new triple, and we
        // have a lease and a release: so six `workspace.jj_op` entries on
        // the timeline.
        let jj_ops: Vec<&serde_json::Value> = events
            .iter()
            .filter(|e| e["event"] == "workspace.jj_op")
            .collect();
        assert_eq!(
            jj_ops.len(),
            6,
            "expected 6 workspace.jj_op events (fetch+bookmark-set+new each for lease+release)"
        );
        let workspace_path_str = workspace_path.display().to_string();
        for op in &jj_ops {
            assert_eq!(op["workspace_path"], workspace_path_str);
        }
    }

    #[test]
    fn workspace_release_by_workspace_id_resolves_active_lease() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let workspace_path = workspace_root.join("mono-agent-004");
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        let lease = Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]);
        run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
        lease_runner.assert_exhausted();

        let release_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            gc_noop_command(&workspace_path),
            gc_pr_remote_noop_command(&workspace_path),
        ]);
        let release = Cli::parse_from(["cube", "workspace", "release", "mono-agent-004"]);
        let result = run_with_dependencies(release, Some(&database_path), &release_runner)
            .expect("release by id");

        assert_eq!(result.payload["workspace"]["state"], "free");
        assert_eq!(
            result.payload["workspace"]["workspace_id"],
            "mono-agent-004"
        );
        release_runner.assert_exhausted();
    }

    #[test]
    fn workspace_release_by_workspace_id_errors_when_not_leased() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        // sync_workspaces is normally called inside lease, so trigger it
        // via list with the registry knowing about this workspace.
        let list = Cli::parse_from(["cube", "workspace", "list", "--repo", "mono"]);
        let _ = run_with_dependencies(list, Some(&database_path), &FakeRunner::default());

        let release = Cli::parse_from(["cube", "workspace", "release", "mono-agent-004"]);
        let error = run_with_dependencies(release, Some(&database_path), &FakeRunner::default())
            .expect_err("release should fail");
        // Workspace id is unknown to the registry until something has synced
        // it, so this surfaces as WorkspaceNotFound.
        assert!(matches!(error, CubeError::WorkspaceNotFound(_)));
    }

    #[test]
    fn workspace_release_keep_dirty_skips_reset_and_records_reason() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let workspace_path = workspace_root.join("mono-agent-001");
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        let lease_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        let lease_id = lease_result.payload["workspace"]["lease_id"]
            .as_str()
            .expect("lease id")
            .to_string();
        lease_runner.assert_exhausted();

        // No reset commands expected — --keep-dirty short-circuits the
        // jj git fetch / jj new main pair.
        let release_runner = FakeRunner::default();
        let result = run_with_dependencies(
            Cli::parse_from([
                "cube",
                "workspace",
                "release",
                "--lease",
                &lease_id,
                "--reason",
                "crash",
                "--keep-dirty",
            ]),
            Some(&database_path),
            &release_runner,
        )
        .expect("release");

        assert_eq!(result.payload["workspace"]["state"], "free");
        assert_eq!(result.payload["workspace"]["last_release_reason"], "crash");
        assert!(result.message.contains("kept dirty"));
        release_runner.assert_exhausted();
    }

    #[test]
    fn workspace_force_release_skips_reset() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let workspace_path = workspace_root.join("mono-agent-001");
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        let lease_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        let lease_id = lease_result.payload["workspace"]["lease_id"]
            .as_str()
            .expect("lease id")
            .to_string();

        // Force-release runs no shell commands.
        let release_runner = FakeRunner::default();
        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "force-release", "--lease", &lease_id]),
            Some(&database_path),
            &release_runner,
        )
        .expect("force-release");

        assert_eq!(result.payload["workspace"]["state"], "free");
        assert_eq!(
            result.payload["workspace"]["last_release_reason"],
            "force-released"
        );
        release_runner.assert_exhausted();
    }

    #[test]
    fn workspace_remove_deletes_synced_free_row() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-007");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            Cli::parse_from([
                "cube",
                "repo",
                "add",
                "mono",
                "--origin",
                "git@github.com:spinyfin/mono.git",
                "--workspace-root",
                &workspace_root.display().to_string(),
                "--workspace-prefix",
                "mono-agent-",
            ]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // Sync the workspace into the registry by listing.
        // (sync runs as a side effect of operations like lease; here we
        // seed the row directly.)
        {
            use crate::metadata::WorkspaceCandidate;
            use crate::store::Store;
            let mut store = Store::open_at(&database_path).unwrap();
            store
                .sync_workspaces(
                    "mono",
                    &[WorkspaceCandidate {
                        workspace_id: "mono-agent-007".to_string(),
                        workspace_path: workspace_path.clone(),
                    }],
                )
                .unwrap();
        }

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "remove", "mono-agent-007"]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("remove");

        assert!(result.message.contains("Removed mono/mono-agent-007"));
        assert_eq!(result.payload["forced"], false);
        assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-007");

        // Row must be gone, but the on-disk directory must remain.
        use crate::store::{Store, WorkspaceListFilter};
        let store = Store::open_at(&database_path).unwrap();
        let remaining = store
            .list_workspaces_filtered(&WorkspaceListFilter {
                repo: Some("mono"),
                ..Default::default()
            })
            .unwrap();
        assert!(remaining.is_empty(), "expected row to be deleted");
        assert!(workspace_path.is_dir(), "directory must be left intact");
    }

    #[test]
    fn workspace_remove_refuses_leased_row_without_force() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("workspace dir");

        run_with_dependencies(
            Cli::parse_from([
                "cube",
                "repo",
                "add",
                "mono",
                "--origin",
                "git@github.com:spinyfin/mono.git",
                "--workspace-root",
                &workspace_root.display().to_string(),
                "--workspace-prefix",
                "mono-agent-",
            ]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let workspace_path = workspace_root.join("mono-agent-001");
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        lease_runner.assert_exhausted();

        let error = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "remove", "mono-agent-001"]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect_err("remove should refuse a leased row");

        match error {
            CubeError::InvalidArgument(msg) => {
                assert!(
                    msg.contains("currently leased"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }

        // Row must still be present.
        use crate::store::{Store, WorkspaceListFilter};
        let store = Store::open_at(&database_path).unwrap();
        let remaining = store
            .list_workspaces_filtered(&WorkspaceListFilter {
                repo: Some("mono"),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn workspace_remove_force_removes_leased_row() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("workspace dir");

        run_with_dependencies(
            Cli::parse_from([
                "cube",
                "repo",
                "add",
                "mono",
                "--origin",
                "git@github.com:spinyfin/mono.git",
                "--workspace-root",
                &workspace_root.display().to_string(),
                "--workspace-prefix",
                "mono-agent-",
            ]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let workspace_path = workspace_root.join("mono-agent-001");
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "remove", "mono-agent-001", "--force"]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("force remove");

        assert_eq!(result.payload["forced"], true);
        assert_eq!(result.payload["workspace"]["state"], "leased");

        use crate::store::{Store, WorkspaceListFilter};
        let store = Store::open_at(&database_path).unwrap();
        let remaining = store
            .list_workspaces_filtered(&WorkspaceListFilter {
                repo: Some("mono"),
                ..Default::default()
            })
            .unwrap();
        assert!(remaining.is_empty(), "row should be deleted under --force");
    }

    #[test]
    fn workspace_remove_succeeds_when_directory_is_gone() {
        // Canonical scenario: the operator wiped the workspace directory
        // by hand and `cube workspace list` still surfaces the row. Remove
        // must succeed without touching the missing path.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-007");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            Cli::parse_from([
                "cube",
                "repo",
                "add",
                "mono",
                "--origin",
                "git@github.com:spinyfin/mono.git",
                "--workspace-root",
                &workspace_root.display().to_string(),
                "--workspace-prefix",
                "mono-agent-",
            ]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        {
            use crate::metadata::WorkspaceCandidate;
            use crate::store::Store;
            let mut store = Store::open_at(&database_path).unwrap();
            store
                .sync_workspaces(
                    "mono",
                    &[WorkspaceCandidate {
                        workspace_id: "mono-agent-007".to_string(),
                        workspace_path: workspace_path.clone(),
                    }],
                )
                .unwrap();
        }

        // Wipe the directory like the user did manually.
        std::fs::remove_dir_all(&workspace_path).unwrap();

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "remove", "mono-agent-007"]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("remove dangling row");

        assert!(result.message.contains("mono-agent-007"));

        use crate::store::{Store, WorkspaceListFilter};
        let store = Store::open_at(&database_path).unwrap();
        let remaining = store
            .list_workspaces_filtered(&WorkspaceListFilter {
                repo: Some("mono"),
                ..Default::default()
            })
            .unwrap();
        assert!(remaining.is_empty());
    }

    #[test]
    fn workspace_remove_errors_when_workspace_id_unknown() {
        let (_tempdir, database_path) = with_database_path();

        let error = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "remove", "mono-agent-999"]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect_err("remove should fail for unknown workspace");

        assert!(matches!(error, CubeError::WorkspaceNotFound(_)));
    }

    #[test]
    fn workspace_remove_emits_audit_entry() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-007");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            Cli::parse_from([
                "cube",
                "repo",
                "add",
                "mono",
                "--origin",
                "git@github.com:spinyfin/mono.git",
                "--workspace-root",
                &workspace_root.display().to_string(),
                "--workspace-prefix",
                "mono-agent-",
            ]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        {
            use crate::metadata::WorkspaceCandidate;
            use crate::store::Store;
            let mut store = Store::open_at(&database_path).unwrap();
            store
                .sync_workspaces(
                    "mono",
                    &[WorkspaceCandidate {
                        workspace_id: "mono-agent-007".to_string(),
                        workspace_path: workspace_path.clone(),
                    }],
                )
                .unwrap();
        }

        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "remove", "mono-agent-007"]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("remove");

        let audit_dir = tempdir.path().join("audit");
        let audit_files: Vec<_> = std::fs::read_dir(&audit_dir)
            .expect("audit dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        assert_eq!(audit_files.len(), 1, "expected one weekly audit file");

        let contents = std::fs::read_to_string(&audit_files[0]).expect("audit content");
        let line = contents.lines().last().expect("at least one event");
        let event: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(event["event"], "workspace.removed");
        assert_eq!(event["repo"], "mono");
        assert_eq!(event["workspace_id"], "mono-agent-007");
        assert_eq!(event["prior_state"], "free");
        assert_eq!(event["forced"], false);
        assert_eq!(event["expunged"], false);
    }

    #[test]
    fn workspace_remove_expunge_deletes_row_and_directory() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-007");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");
        std::fs::write(workspace_path.join("marker"), "x").expect("marker file");

        run_with_dependencies(
            Cli::parse_from([
                "cube",
                "repo",
                "add",
                "mono",
                "--origin",
                "git@github.com:spinyfin/mono.git",
                "--workspace-root",
                &workspace_root.display().to_string(),
                "--workspace-prefix",
                "mono-agent-",
            ]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        {
            use crate::metadata::WorkspaceCandidate;
            use crate::store::Store;
            let mut store = Store::open_at(&database_path).unwrap();
            store
                .sync_workspaces(
                    "mono",
                    &[WorkspaceCandidate {
                        workspace_id: "mono-agent-007".to_string(),
                        workspace_path: workspace_path.clone(),
                    }],
                )
                .unwrap();
        }

        let result = run_with_dependencies(
            Cli::parse_from([
                "cube",
                "workspace",
                "remove",
                "mono-agent-007",
                "--expunge",
            ]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("expunge remove");

        assert_eq!(result.payload["expunged"], true);
        assert!(result.message.contains("deleted workspace directory"));
        assert!(
            !workspace_path.exists(),
            "expected on-disk directory to be removed"
        );

        use crate::store::{Store, WorkspaceListFilter};
        let store = Store::open_at(&database_path).unwrap();
        let remaining = store
            .list_workspaces_filtered(&WorkspaceListFilter {
                repo: Some("mono"),
                ..Default::default()
            })
            .unwrap();
        assert!(remaining.is_empty(), "row should be deleted");
    }

    #[test]
    fn workspace_remove_expunge_tolerates_missing_directory() {
        // The directory may already be gone (operator wiped it manually);
        // --expunge should still succeed and clean up the row.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-007");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            Cli::parse_from([
                "cube",
                "repo",
                "add",
                "mono",
                "--origin",
                "git@github.com:spinyfin/mono.git",
                "--workspace-root",
                &workspace_root.display().to_string(),
                "--workspace-prefix",
                "mono-agent-",
            ]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        {
            use crate::metadata::WorkspaceCandidate;
            use crate::store::Store;
            let mut store = Store::open_at(&database_path).unwrap();
            store
                .sync_workspaces(
                    "mono",
                    &[WorkspaceCandidate {
                        workspace_id: "mono-agent-007".to_string(),
                        workspace_path: workspace_path.clone(),
                    }],
                )
                .unwrap();
        }

        std::fs::remove_dir_all(&workspace_path).unwrap();

        let result = run_with_dependencies(
            Cli::parse_from([
                "cube",
                "workspace",
                "remove",
                "mono-agent-007",
                "--expunge",
            ]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("expunge tolerates missing dir");

        assert_eq!(result.payload["expunged"], true);
        assert!(!workspace_path.exists());
    }

    #[test]
    fn workspace_remove_without_expunge_leaves_directory_intact() {
        // Regression check on PR #291's default behaviour: omitting
        // --expunge must keep the on-disk workspace directory.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-007");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");
        std::fs::write(workspace_path.join("marker"), "x").expect("marker file");

        run_with_dependencies(
            Cli::parse_from([
                "cube",
                "repo",
                "add",
                "mono",
                "--origin",
                "git@github.com:spinyfin/mono.git",
                "--workspace-root",
                &workspace_root.display().to_string(),
                "--workspace-prefix",
                "mono-agent-",
            ]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        {
            use crate::metadata::WorkspaceCandidate;
            use crate::store::Store;
            let mut store = Store::open_at(&database_path).unwrap();
            store
                .sync_workspaces(
                    "mono",
                    &[WorkspaceCandidate {
                        workspace_id: "mono-agent-007".to_string(),
                        workspace_path: workspace_path.clone(),
                    }],
                )
                .unwrap();
        }

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "remove", "mono-agent-007"]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("remove");

        assert_eq!(result.payload["expunged"], false);
        assert!(
            workspace_path.is_dir(),
            "directory must remain when --expunge is not passed"
        );
        assert!(
            workspace_path.join("marker").is_file(),
            "directory contents must be preserved"
        );
    }

    #[test]
    fn workspace_remove_expunge_makes_removal_durable_against_lease_resync() {
        // After --expunge, a follow-up lease's discover/sync round must
        // NOT resurrect the row (that was the gap that motivated the
        // flag).
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-007");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            Cli::parse_from([
                "cube",
                "repo",
                "add",
                "mono",
                "--origin",
                "git@github.com:spinyfin/mono.git",
                "--workspace-root",
                &workspace_root.display().to_string(),
                "--workspace-prefix",
                "mono-agent-",
            ]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        {
            use crate::metadata::WorkspaceCandidate;
            use crate::store::Store;
            let mut store = Store::open_at(&database_path).unwrap();
            store
                .sync_workspaces(
                    "mono",
                    &[WorkspaceCandidate {
                        workspace_id: "mono-agent-007".to_string(),
                        workspace_path: workspace_path.clone(),
                    }],
                )
                .unwrap();
        }

        run_with_dependencies(
            Cli::parse_from([
                "cube",
                "workspace",
                "remove",
                "mono-agent-007",
                "--expunge",
            ]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("expunge remove");

        // A subsequent lease must not see the just-expunged workspace.
        // It will auto-create a fresh `mono-agent-001` instead via the
        // FakeRunner's `jj git clone` expectation. (The fake runner just
        // records the invocation; we manually create the resulting
        // directory.)
        let new_path = workspace_root.join("mono-agent-001");
        let staging = workspace_root.join(".incoming-mono-agent-001");
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "--colocate",
                    "git@github.com:spinyfin/mono.git",
                    &staging.display().to_string(),
                ],
                "",
            )
            .creating_dir(staging.clone()),
            ExpectedCommand::ok(
                staging.clone(),
                "jj",
                &["bookmark", "track", "main@origin"],
                "",
            ),
            ExpectedCommand::no_such_remote_bookmark(
                staging.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
            ),
            ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                new_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);

        let lease_result = run_with_dependencies(
            Cli::parse_from([
                "cube",
                "workspace",
                "lease",
                "mono",
                "--task",
                "after-expunge",
            ]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease after expunge");

        assert_eq!(
            lease_result.payload["workspace"]["workspace_id"],
            "mono-agent-001",
            "lease should auto-create a fresh slot, not resurrect the expunged one"
        );

        use crate::store::{Store, WorkspaceListFilter};
        let store = Store::open_at(&database_path).unwrap();
        let rows = store
            .list_workspaces_filtered(&WorkspaceListFilter {
                repo: Some("mono"),
                ..Default::default()
            })
            .unwrap();
        let ids: Vec<_> = rows.iter().map(|r| r.workspace_id.as_str()).collect();
        assert!(
            !ids.contains(&"mono-agent-007"),
            "expunged workspace must not reappear; saw {ids:?}"
        );
    }

    #[test]
    fn workspace_remove_without_expunge_resurrects_on_next_lease() {
        // Documents the without-expunge gap: PR #291 removed the row but
        // left the directory, so the next lease's discover/sync brings
        // the row back as state=Free. This test pins that behaviour;
        // operators who want durable removal must use --expunge.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-007");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            Cli::parse_from([
                "cube",
                "repo",
                "add",
                "mono",
                "--origin",
                "git@github.com:spinyfin/mono.git",
                "--workspace-root",
                &workspace_root.display().to_string(),
                "--workspace-prefix",
                "mono-agent-",
            ]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        {
            use crate::metadata::WorkspaceCandidate;
            use crate::store::Store;
            let mut store = Store::open_at(&database_path).unwrap();
            store
                .sync_workspaces(
                    "mono",
                    &[WorkspaceCandidate {
                        workspace_id: "mono-agent-007".to_string(),
                        workspace_path: workspace_path.clone(),
                    }],
                )
                .unwrap();
        }

        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "remove", "mono-agent-007"]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("remove");

        // Without --expunge the dir is still there, so the next lease
        // discovers it and re-syncs the row.
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        let lease_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "resync"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease re-syncs row");

        assert_eq!(
            lease_result.payload["workspace"]["workspace_id"],
            "mono-agent-007",
            "without --expunge the discovered directory resurrects the row"
        );
    }

    #[test]
    fn workspace_heartbeat_extends_expiry() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let workspace_path = workspace_root.join("mono-agent-001");
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        let lease_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        let lease_id = lease_result.payload["workspace"]["lease_id"]
            .as_str()
            .expect("lease id")
            .to_string();
        let before_expiry = lease_result.payload["workspace"]["lease_expires_at_epoch_s"]
            .as_i64()
            .expect("initial expiry");

        // Sleep a touch so wall-clock current_epoch_s advances; the
        // heartbeat handler uses it as the new base.
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Heartbeat with the default TTL (1800s) — since current_epoch_s
        // moved forward by >1s since the lease, the new expiry must be
        // strictly greater than the initial one.
        let beat_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "heartbeat", "--lease", &lease_id]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("heartbeat");

        let after_expiry = beat_result.payload["workspace"]["lease_expires_at_epoch_s"]
            .as_i64()
            .expect("new expiry");
        assert!(
            after_expiry > before_expiry,
            "heartbeat should advance expiry: before={before_expiry}, after={after_expiry}"
        );

        // Also confirm a custom shorter TTL is honored exactly.
        let custom = run_with_dependencies(
            Cli::parse_from([
                "cube",
                "workspace",
                "heartbeat",
                "--lease",
                &lease_id,
                "--ttl-seconds",
                "60",
            ]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("heartbeat custom");
        let custom_expiry = custom.payload["workspace"]["lease_expires_at_epoch_s"]
            .as_i64()
            .expect("custom expiry");
        let now_after = current_epoch_s().expect("now");
        // expiry should be ~60s after the call; allow some slack for slow runners.
        let delta = custom_expiry - now_after;
        assert!(
            (delta - 60).abs() <= 5,
            "custom expiry {custom_expiry} should be ~now+60={}, delta {delta}s",
            now_after + 60
        );
    }

    #[test]
    fn workspace_status_includes_jj_status_output() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let workspace_path = workspace_root.join("mono-agent-004");
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "implement cube",
        ]);
        run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
        lease_runner.assert_exhausted();

        let status_runner = FakeRunner::new(vec![ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["status"],
            "The working copy is clean",
        )]);
        let status = Cli::parse_from([
            "cube",
            "workspace",
            "status",
            "--workspace",
            &workspace_path.display().to_string(),
        ]);
        let status_result =
            run_with_dependencies(status, Some(&database_path), &status_runner).expect("status");

        assert_eq!(
            status_result.payload["jj_status"],
            "The working copy is clean"
        );
        assert!(status_result.message.contains("jj_status:"));
        status_runner.assert_exhausted();
    }

    #[test]
    fn workspace_status_forgets_missing_workspace_rows() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let workspace_path = workspace_root.join("mono-agent-004");
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "implement cube",
        ]);
        run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
        lease_runner.assert_exhausted();

        std::fs::remove_dir_all(&workspace_path).expect("remove workspace dir");

        let status = Cli::parse_from([
            "cube",
            "workspace",
            "status",
            "--workspace",
            &workspace_path.display().to_string(),
        ]);
        let error = run_with_dependencies(status, Some(&database_path), &FakeRunner::default())
            .expect_err("status should forget missing workspace");

        assert!(matches!(error, CubeError::WorkspaceNotFound(_)));
    }

    #[test]
    fn workspace_list_returns_filtered_rows() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("workspace dir");
        std::fs::create_dir_all(workspace_root.join("mono-agent-002").join(".jj")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let first_path = workspace_root.join("mono-agent-001");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(first_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(first_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(first_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(first_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                first_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        let lease = Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]);
        run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

        // global list returns both rows
        let list_all = Cli::parse_from(["cube", "workspace", "list"]);
        let result_all =
            run_with_dependencies(list_all, Some(&database_path), &FakeRunner::default())
                .expect("list");
        let rows = result_all.payload["workspaces"].as_array().expect("array");
        assert_eq!(rows.len(), 2);

        // state filter narrows to leased only
        let list_leased = Cli::parse_from(["cube", "workspace", "list", "--state", "leased"]);
        let result_leased =
            run_with_dependencies(list_leased, Some(&database_path), &FakeRunner::default())
                .expect("list leased");
        let leased = result_leased.payload["workspaces"]
            .as_array()
            .expect("array");
        assert_eq!(leased.len(), 1);
        assert_eq!(leased[0]["workspace_id"], "mono-agent-001");
        assert_eq!(leased[0]["state"], "leased");
        assert_eq!(leased[0]["task"], "demo");

        // invalid state returns argument error
        let list_bad = Cli::parse_from(["cube", "workspace", "list", "--state", "bogus"]);
        let error = run_with_dependencies(list_bad, Some(&database_path), &FakeRunner::default())
            .expect_err("invalid state");
        assert!(matches!(error, CubeError::InvalidArgument(_)));
    }

    #[test]
    fn change_create_records_named_workspace_head() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let workspace_path = workspace_root.join("mono-agent-004");
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "implement cube",
        ]);
        run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
        lease_runner.assert_exhausted();

        let change_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["describe", "-m", "Implement parser"],
                "",
            ),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &[
                    "log",
                    "--no-graph",
                    "-r",
                    "@",
                    "-T",
                    "change_id ++ \"\\n\" ++ commit_id.short()",
                ],
                "zxy123\nabc1234",
            ),
        ]);
        let create = Cli::parse_from([
            "cube",
            "change",
            "create",
            "--workspace",
            &workspace_path.display().to_string(),
            "--title",
            "Implement parser",
        ]);
        let result =
            run_with_dependencies(create, Some(&database_path), &change_runner).expect("change");

        assert_eq!(result.payload["change"]["repo"], "mono");
        assert_eq!(
            result.payload["change"]["workspace_path"],
            workspace_path.display().to_string()
        );
        assert_eq!(result.payload["change"]["title"], "Implement parser");
        assert_eq!(result.payload["change"]["jj_change_id"], "zxy123");
        assert_eq!(result.payload["change"]["head_commit"], "abc1234");
        change_runner.assert_exhausted();
    }

    #[test]
    fn change_create_from_parent_uses_parent_jj_change_id() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let workspace_path = workspace_root.join("mono-agent-004");
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "implement cube",
        ]);
        run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
        lease_runner.assert_exhausted();

        let root_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["describe", "-m", "Implement parser"],
                "",
            ),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &[
                    "log",
                    "--no-graph",
                    "-r",
                    "@",
                    "-T",
                    "change_id ++ \"\\n\" ++ commit_id.short()",
                ],
                "root123\nabc1234",
            ),
        ]);
        let root = Cli::parse_from([
            "cube",
            "change",
            "create",
            "--workspace",
            &workspace_path.display().to_string(),
            "--title",
            "Implement parser",
        ]);
        let root_result =
            run_with_dependencies(root, Some(&database_path), &root_runner).expect("root change");
        root_runner.assert_exhausted();
        let parent_change_id = root_result.payload["change"]["change_id"]
            .as_str()
            .expect("parent change id")
            .to_string();

        let child_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["new", "root123", "-m", "Add tests"],
                "",
            ),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &[
                    "log",
                    "--no-graph",
                    "-r",
                    "@",
                    "-T",
                    "change_id ++ \"\\n\" ++ commit_id.short()",
                ],
                "child456\nbcd2345",
            ),
        ]);
        let child = Cli::parse_from([
            "cube",
            "change",
            "create",
            "--parent",
            &parent_change_id,
            "--title",
            "Add tests",
        ]);
        let child_result =
            run_with_dependencies(child, Some(&database_path), &child_runner).expect("child");

        assert_eq!(
            child_result.payload["change"]["parent_change_id"],
            parent_change_id
        );
        assert_eq!(child_result.payload["change"]["jj_change_id"], "child456");
        child_runner.assert_exhausted();
    }

    #[test]
    fn change_info_round_trips_record() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

        let add = Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ]);
        run_with_dependencies(add, Some(&database_path), &FakeRunner::default()).expect("repo");

        let workspace_path = workspace_root.join("mono-agent-004");
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);
        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "implement cube",
        ]);
        run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
        lease_runner.assert_exhausted();

        let change_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["describe", "-m", "Implement parser"],
                "",
            ),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &[
                    "log",
                    "--no-graph",
                    "-r",
                    "@",
                    "-T",
                    "change_id ++ \"\\n\" ++ commit_id.short()",
                ],
                "zxy123\nabc1234",
            ),
        ]);
        let create = Cli::parse_from([
            "cube",
            "change",
            "create",
            "--workspace",
            &workspace_path.display().to_string(),
            "--title",
            "Implement parser",
        ]);
        let create_result =
            run_with_dependencies(create, Some(&database_path), &change_runner).expect("change");
        change_runner.assert_exhausted();

        let change_id = create_result.payload["change"]["change_id"]
            .as_str()
            .expect("change id")
            .to_string();
        let info = Cli::parse_from(["cube", "change", "info", "--change", &change_id]);
        let info_result = run_with_dependencies(info, Some(&database_path), &FakeRunner::default())
            .expect("info");

        assert_eq!(info_result.payload["change"]["change_id"], change_id);
        assert_eq!(info_result.payload["change"]["title"], "Implement parser");
    }

    fn write_setup_yaml(workspace_path: &std::path::Path, body: &str) {
        let path = workspace_path.join(".cube/setup.yaml");
        std::fs::create_dir_all(path.parent().unwrap()).expect("setup dir");
        std::fs::write(&path, body).expect("setup yaml");
    }

    fn lease_runner_for(workspace_path: &std::path::Path, head: &str) -> FakeRunner {
        FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.to_path_buf(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                head,
            ),
        ])
    }

    /// Returns an expected gc-log command that reports no consumed bookmarks.
    /// Add to any release runner after `jj new main` to satisfy the gc check.
    fn gc_noop_command(workspace_path: &std::path::Path) -> ExpectedCommand {
        ExpectedCommand::ok(
            workspace_path.to_path_buf(),
            "jj",
            &[
                "log",
                "-r",
                "bookmarks(glob:\"boss/exec_*\") & ::main",
                "--no-graph",
                "-T",
                "bookmarks ++ \"\\n\"",
            ],
            "",
        )
    }

    /// Command that satisfies the `gc_collect_closed_pr_bookmarks` remote-list
    /// probe with a non-GitHub remote, causing the pr/* sweep to skip.
    fn gc_pr_remote_noop_command(workspace_path: &std::path::Path) -> ExpectedCommand {
        ExpectedCommand::ok(
            workspace_path.to_path_buf(),
            "jj",
            &["git", "remote", "list"],
            "origin /local/path/to/mirror\n",
        )
    }

    /// Standard release runner: fetch, reset, then gc-noop (exec + pr sweeps).
    fn release_runner_for(workspace_path: &std::path::Path) -> FakeRunner {
        FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["new", "main"], ""),
            gc_noop_command(workspace_path),
            gc_pr_remote_noop_command(workspace_path),
        ])
    }

    fn lease_runner_with_setup(
        workspace_path: &std::path::Path,
        head: &str,
        setup_steps: Vec<ExpectedCommand>,
    ) -> FakeRunner {
        let mut commands = vec![
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.to_path_buf(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                head,
            ),
        ];
        commands.extend(setup_steps);
        FakeRunner::new(commands)
    }

    fn add_repo_cli(workspace_root: &std::path::Path) -> Cli {
        Cli::parse_from([
            "cube",
            "repo",
            "add",
            "mono",
            "--origin",
            "git@github.com:spinyfin/mono.git",
            "--workspace-root",
            &workspace_root.display().to_string(),
            "--workspace-prefix",
            "mono-agent-",
        ])
    }

    #[test]
    fn workspace_setup_returns_empty_when_no_setup_yaml() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(workspace_path.join(".jj")).unwrap();

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let lease_runner = lease_runner_for(&workspace_path, "abc1234");
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        lease_runner.assert_exhausted();

        let setup_runner = FakeRunner::default();
        let setup = Cli::parse_from([
            "cube",
            "workspace",
            "setup",
            "--workspace",
            &workspace_path.display().to_string(),
        ]);
        let result =
            run_with_dependencies(setup, Some(&database_path), &setup_runner).expect("setup");
        setup_runner.assert_exhausted();
        assert_eq!(
            result.message,
            "No setup steps are configured for mono-agent-001."
        );
        assert_eq!(result.payload["setup"]["steps"], json!([]));
    }

    // ── gc tests ─────────────────────────────────────────────────────────────

    #[test]
    fn workspace_release_gc_forgets_consumed_bookmarks() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let lease_runner = lease_runner_for(&workspace_path, "abc1234");
        let lease_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "gc test"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        lease_runner.assert_exhausted();
        let lease_id = lease_result.payload["workspace"]["lease_id"]
            .as_str()
            .unwrap()
            .to_string();

        // Release runner returns a consumed bookmark from the gc log query.
        let release_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &[
                    "log",
                    "-r",
                    "bookmarks(glob:\"boss/exec_*\") & ::main",
                    "--no-graph",
                    "-T",
                    "bookmarks ++ \"\\n\"",
                ],
                "boss/exec_18abcd_01",
            ),
            gc_pr_remote_noop_command(&workspace_path),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["bookmark", "forget", "boss/exec_18abcd_01"],
                "",
            ),
        ]);
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
            Some(&database_path),
            &release_runner,
        )
        .expect("release");
        release_runner.assert_exhausted();
    }

    #[test]
    fn workspace_gc_verb_forgets_consumed_bookmarks_on_free_workspaces() {
        // Two workspaces: 001 gets leased (skipped by gc), 002 stays free (gc'd).
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let ws1_path = workspace_root.join("mono-agent-001"); // will be leased
        let ws2_path = workspace_root.join("mono-agent-002"); // stays free
        std::fs::create_dir_all(ws1_path.join(".jj")).expect("ws1 dir");
        std::fs::create_dir_all(ws2_path.join(".jj")).expect("ws2 dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // Lease ws1 (001) — picks it first since it's clean. This also syncs
        // ws2 into the registry as free.
        let lease_runner = lease_runner_for(&ws1_path, "abc1234");
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "keep leased"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        lease_runner.assert_exhausted();

        // gc: ws1 is leased → skipped; ws2 is free → fetch + forget.
        let gc_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(ws2_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(
                ws2_path.clone(),
                "jj",
                &[
                    "log",
                    "-r",
                    "bookmarks(glob:\"boss/exec_*\") & ::main",
                    "--no-graph",
                    "-T",
                    "bookmarks ++ \"\\n\"",
                ],
                "boss/exec_dead_01",
            ),
            gc_pr_remote_noop_command(&ws2_path),
            ExpectedCommand::ok(
                ws2_path.clone(),
                "jj",
                &["bookmark", "forget", "boss/exec_dead_01"],
                "",
            ),
        ]);
        let gc_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "gc"]),
            Some(&database_path),
            &gc_runner,
        )
        .expect("gc");
        gc_runner.assert_exhausted();

        let results = gc_result.payload["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);

        let ws1_r = results
            .iter()
            .find(|r| r["workspace_id"] == "mono-agent-001")
            .unwrap();
        assert_eq!(ws1_r["skipped"], true);
        assert_eq!(ws1_r["skipped_reason"], "leased");

        let ws2_r = results
            .iter()
            .find(|r| r["workspace_id"] == "mono-agent-002")
            .unwrap();
        assert_eq!(ws2_r["skipped"], false);
        assert_eq!(ws2_r["bookmarks_forgotten"].as_array().unwrap().len(), 1);
        assert_eq!(ws2_r["bookmarks_forgotten"][0], "boss/exec_dead_01");
    }

    #[test]
    fn workspace_gc_dry_run_lists_without_forgetting() {
        // dry-run: fetch + log are called, but bookmark forget is NOT.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // Lease then release to get the workspace into the registry as free.
        let lease_runner = lease_runner_for(&workspace_path, "abc1234");
        let lease_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "seed"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        lease_runner.assert_exhausted();
        let lease_id = lease_result.payload["workspace"]["lease_id"]
            .as_str()
            .unwrap()
            .to_string();
        let release_runner = release_runner_for(&workspace_path);
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
            Some(&database_path),
            &release_runner,
        )
        .expect("release");
        release_runner.assert_exhausted();

        // dry-run: fetch + log, but NO bookmark forget.
        let gc_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &[
                    "log",
                    "-r",
                    "bookmarks(glob:\"boss/exec_*\") & ::main",
                    "--no-graph",
                    "-T",
                    "bookmarks ++ \"\\n\"",
                ],
                "boss/exec_dry_01",
            ),
            gc_pr_remote_noop_command(&workspace_path),
        ]);
        let gc_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "gc", "--dry-run"]),
            Some(&database_path),
            &gc_runner,
        )
        .expect("gc dry-run");
        gc_runner.assert_exhausted();

        assert!(gc_result.message.contains("dry-run"));
        let results = gc_result.payload["results"].as_array().unwrap();
        assert_eq!(results[0]["bookmarks_forgotten"].as_array().unwrap().len(), 1);
        assert_eq!(results[0]["bookmarks_forgotten"][0], "boss/exec_dry_01");
    }

    #[test]
    fn gc_forgets_closed_pr_bookmark() {
        // A pr/42 bookmark whose GitHub PR is CLOSED is forgotten by gc.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let lease_runner = lease_runner_for(&workspace_path, "abc1234");
        let lease_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "pr gc test"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        lease_runner.assert_exhausted();
        let lease_id = lease_result.payload["workspace"]["lease_id"]
            .as_str()
            .unwrap()
            .to_string();

        // Release runner: gc finds a closed pr/42 bookmark and forgets it.
        let release_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            // exec sweep: no consumed exec bookmarks.
            gc_noop_command(&workspace_path),
            // pr sweep: GitHub remote resolved, pr/42 found, state = CLOSED.
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "remote", "list"],
                "github\tgit@github.com:spinyfin/mono.git\norigin\t/local/mirror\n"),
            ExpectedCommand::ok(
                workspace_path.clone(), "jj",
                &["log", "-r", "bookmarks(glob:\"pr/*\")", "--no-graph", "-T", "bookmarks ++ \"\\n\""],
                "pr/42\n",
            ),
            ExpectedCommand::ok(
                workspace_path.clone(), "gh",
                &["pr", "view", "42", "-R", "spinyfin/mono", "--json", "state", "--jq", ".state"],
                "CLOSED",
            ),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "forget", "pr/42"], ""),
        ]);
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
            Some(&database_path),
            &release_runner,
        )
        .expect("release");
        release_runner.assert_exhausted();
    }

    #[test]
    fn gc_forgets_merged_pr_bookmark() {
        // A pr/99 bookmark whose GitHub PR is MERGED is forgotten by gc.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let lease_runner = lease_runner_for(&workspace_path, "abc1234");
        let lease_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "pr gc merged"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        lease_runner.assert_exhausted();
        let lease_id = lease_result.payload["workspace"]["lease_id"]
            .as_str()
            .unwrap()
            .to_string();

        let release_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            gc_noop_command(&workspace_path),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "remote", "list"],
                "github\tgit@github.com:spinyfin/mono.git\norigin\t/local/mirror\n"),
            ExpectedCommand::ok(
                workspace_path.clone(), "jj",
                &["log", "-r", "bookmarks(glob:\"pr/*\")", "--no-graph", "-T", "bookmarks ++ \"\\n\""],
                "pr/99\n",
            ),
            ExpectedCommand::ok(
                workspace_path.clone(), "gh",
                &["pr", "view", "99", "-R", "spinyfin/mono", "--json", "state", "--jq", ".state"],
                "MERGED",
            ),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "forget", "pr/99"], ""),
        ]);
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
            Some(&database_path),
            &release_runner,
        )
        .expect("release");
        release_runner.assert_exhausted();
    }

    #[test]
    fn gc_retains_open_pr_bookmark() {
        // A pr/7 bookmark whose GitHub PR is still OPEN is NOT forgotten.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let lease_runner = lease_runner_for(&workspace_path, "abc1234");
        let lease_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "pr gc open"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        lease_runner.assert_exhausted();
        let lease_id = lease_result.payload["workspace"]["lease_id"]
            .as_str()
            .unwrap()
            .to_string();

        // Release runner: gc finds pr/7 but state is OPEN — no forget call.
        let release_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            gc_noop_command(&workspace_path),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "remote", "list"],
                "github\tgit@github.com:spinyfin/mono.git\norigin\t/local/mirror\n"),
            ExpectedCommand::ok(
                workspace_path.clone(), "jj",
                &["log", "-r", "bookmarks(glob:\"pr/*\")", "--no-graph", "-T", "bookmarks ++ \"\\n\""],
                "pr/7\n",
            ),
            ExpectedCommand::ok(
                workspace_path.clone(), "gh",
                &["pr", "view", "7", "-R", "spinyfin/mono", "--json", "state", "--jq", ".state"],
                "OPEN",
            ),
            // No bookmark forget — pr/7 is still open.
        ]);
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
            Some(&database_path),
            &release_runner,
        )
        .expect("release");
        release_runner.assert_exhausted();
    }

    #[test]
    fn gc_skips_pr_sweep_when_offline() {
        // When jj git remote list fails, pr/* GC is skipped entirely.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let lease_runner = lease_runner_for(&workspace_path, "abc1234");
        let lease_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "offline gc"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        lease_runner.assert_exhausted();
        let lease_id = lease_result.payload["workspace"]["lease_id"]
            .as_str()
            .unwrap()
            .to_string();

        // Release runner: remote list fails → pr sweep skipped, no extra commands.
        let release_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            gc_noop_command(&workspace_path),
            ExpectedCommand {
                cwd: workspace_path.clone(),
                program: "jj".to_string(),
                args: ["git", "remote", "list"].iter().map(|s| s.to_string()).collect(),
                result: Err(CubeError::CommandFailed {
                    program: "jj".to_string(),
                    args: vec!["git".to_string(), "remote".to_string(), "list".to_string()],
                    status: Some(1),
                    stderr: "no jj repo".to_string(),
                }),
                creates_dir: None,
            },
        ]);
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
            Some(&database_path),
            &release_runner,
        )
        .expect("release");
        release_runner.assert_exhausted();
    }

    #[test]
    fn auto_gc_updates_timestamp_when_stale() {
        // When last_pool_gc_at is older than 24h, lease updates it.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // Set last_pool_gc_at to 25h ago so the next lease triggers gc.
        let old_ts = current_epoch_s().unwrap() - (25 * 60 * 60);
        {
            use crate::store::Store;
            let store = Store::open_at(&database_path).unwrap();
            store.set_pool_metadata_i(POOL_GC_LAST_AT_KEY, old_ts).unwrap();
        }

        let workspace_path = workspace_root.join("mono-agent-001");
        let lease_runner = lease_runner_for(&workspace_path, "abc1234");
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "stale gc test"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        lease_runner.assert_exhausted();

        // last_pool_gc_at must have advanced past old_ts.
        use crate::store::Store;
        let store = Store::open_at(&database_path).unwrap();
        let ts = store
            .get_pool_metadata_i(POOL_GC_LAST_AT_KEY)
            .unwrap()
            .expect("last_pool_gc_at should be set");
        assert!(ts > old_ts, "last_pool_gc_at should have been updated");
        let now = current_epoch_s().unwrap();
        assert!(now - ts < 10, "last_pool_gc_at should be near now");
    }

    #[test]
    fn auto_gc_skips_when_already_ran_within_24h() {
        // When last_pool_gc_at is recent, lease does NOT update it.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // Set last_pool_gc_at to 1h ago — well within 24h.
        let recent_ts = current_epoch_s().unwrap() - 3600;
        {
            use crate::store::Store;
            let store = Store::open_at(&database_path).unwrap();
            store.set_pool_metadata_i(POOL_GC_LAST_AT_KEY, recent_ts).unwrap();
        }

        let workspace_path = workspace_root.join("mono-agent-001");
        let lease_runner = lease_runner_for(&workspace_path, "abc1234");
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "recent gc test"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        lease_runner.assert_exhausted();

        // last_pool_gc_at must NOT have changed.
        use crate::store::Store;
        let store = Store::open_at(&database_path).unwrap();
        let ts = store
            .get_pool_metadata_i(POOL_GC_LAST_AT_KEY)
            .unwrap()
            .expect("last_pool_gc_at should be set");
        assert_eq!(ts, recent_ts, "last_pool_gc_at should NOT change within 24h");
    }

    #[test]
    fn workspace_setup_runs_steps_then_skips_when_fingerprint_unchanged() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(workspace_path.join(".jj")).unwrap();
        std::fs::write(workspace_path.join("pnpm-lock.yaml"), b"v1").unwrap();
        write_setup_yaml(
            &workspace_path,
            r#"version: 1
steps:
  - id: deps
    command: pnpm install --frozen-lockfile
    fingerprint:
      - file: pnpm-lock.yaml
"#,
        );

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // First lease runs the deps step.
        let lease_runner = lease_runner_with_setup(
            &workspace_path,
            "abc1234",
            vec![ExpectedCommand::ok(
                workspace_path.clone(),
                "pnpm",
                &["install", "--frozen-lockfile"],
                "",
            )],
        );
        let lease_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("first lease");
        lease_runner.assert_exhausted();
        let setup_steps = lease_result.payload["setup"]["steps"]
            .as_array()
            .expect("steps array");
        assert_eq!(setup_steps.len(), 1);
        assert_eq!(setup_steps[0]["id"], "deps");
        assert_eq!(setup_steps[0]["status"], "ran");
        let lease_id = lease_result.payload["workspace"]["lease_id"]
            .as_str()
            .unwrap()
            .to_string();

        // Release so we can re-lease cleanly.
        let release_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            gc_noop_command(&workspace_path),
            gc_pr_remote_noop_command(&workspace_path),
        ]);
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
            Some(&database_path),
            &release_runner,
        )
        .expect("release");
        release_runner.assert_exhausted();

        // Second lease: lockfile unchanged, deps step is skipped (no
        // pnpm command in expectations).
        let second_lease_runner = lease_runner_for(&workspace_path, "def5678");
        let second_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo2"]),
            Some(&database_path),
            &second_lease_runner,
        )
        .expect("second lease");
        second_lease_runner.assert_exhausted();
        let second_steps = second_result.payload["setup"]["steps"]
            .as_array()
            .expect("steps array");
        assert_eq!(second_steps.len(), 1);
        assert_eq!(second_steps[0]["status"], "skipped");
        assert_eq!(second_steps[0]["reason"], "fingerprint_unchanged");
    }

    #[test]
    fn workspace_setup_reruns_when_lockfile_changes() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(workspace_path.join(".jj")).unwrap();
        std::fs::write(workspace_path.join("pnpm-lock.yaml"), b"v1").unwrap();
        write_setup_yaml(
            &workspace_path,
            r#"version: 1
steps:
  - id: deps
    command: pnpm install
    fingerprint:
      - file: pnpm-lock.yaml
"#,
        );

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let lease_runner = lease_runner_with_setup(
            &workspace_path,
            "abc1234",
            vec![ExpectedCommand::ok(
                workspace_path.clone(),
                "pnpm",
                &["install"],
                "",
            )],
        );
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("first lease");
        lease_runner.assert_exhausted();

        // Lockfile bumps; re-running setup explicitly (without re-leasing)
        // should pick up the change.
        std::fs::write(workspace_path.join("pnpm-lock.yaml"), b"v2").unwrap();

        let setup_runner = FakeRunner::new(vec![ExpectedCommand::ok(
            workspace_path.clone(),
            "pnpm",
            &["install"],
            "",
        )]);
        let setup_result = run_with_dependencies(
            Cli::parse_from([
                "cube",
                "workspace",
                "setup",
                "--workspace",
                &workspace_path.display().to_string(),
            ]),
            Some(&database_path),
            &setup_runner,
        )
        .expect("setup");
        setup_runner.assert_exhausted();
        let steps = setup_result.payload["setup"]["steps"].as_array().unwrap();
        assert_eq!(steps[0]["status"], "ran");
    }

    #[test]
    fn workspace_setup_on_create_skips_after_first_run() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(workspace_path.join(".jj")).unwrap();
        write_setup_yaml(
            &workspace_path,
            r#"version: 1
steps:
  - id: secrets
    command: ./decode-secrets.sh
    run_when: on-create
"#,
        );

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // First lease: on-create runs once.
        let lease_runner = lease_runner_with_setup(
            &workspace_path,
            "abc1234",
            vec![ExpectedCommand::ok(
                workspace_path.clone(),
                "./decode-secrets.sh",
                &[],
                "",
            )],
        );
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("first lease");
        lease_runner.assert_exhausted();

        // Release before re-leasing the same workspace.
        let workspace_record = {
            use crate::store::Store;
            let store = Store::open_at(&database_path).unwrap();
            store
                .get_workspace_by_path(&workspace_path)
                .unwrap()
                .unwrap()
        };
        let release_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            gc_noop_command(&workspace_path),
            gc_pr_remote_noop_command(&workspace_path),
        ]);
        run_with_dependencies(
            Cli::parse_from([
                "cube",
                "workspace",
                "release",
                "--lease",
                workspace_record.lease_id.as_deref().unwrap(),
            ]),
            Some(&database_path),
            &release_runner,
        )
        .expect("release");
        release_runner.assert_exhausted();

        // Second lease: on-create should skip (no decode-secrets in expectations).
        let second_runner = lease_runner_for(&workspace_path, "def5678");
        let second = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo2"]),
            Some(&database_path),
            &second_runner,
        )
        .expect("second lease");
        second_runner.assert_exhausted();
        let steps = second.payload["setup"]["steps"].as_array().unwrap();
        assert_eq!(steps[0]["status"], "skipped");
        assert_eq!(steps[0]["reason"], "already_ran");
    }

    #[test]
    fn workspace_setup_failure_surfaces_step_id_and_retains_lease() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(workspace_path.join(".jj")).unwrap();
        write_setup_yaml(
            &workspace_path,
            r#"version: 1
steps:
  - id: deps
    command: pnpm install
    run_when: always
"#,
        );

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let failing = ExpectedCommand {
            cwd: workspace_path.clone(),
            program: "pnpm".to_string(),
            args: vec!["install".to_string()],
            result: Err(CubeError::CommandFailed {
                program: "pnpm".to_string(),
                args: vec!["install".to_string()],
                status: Some(1),
                stderr: "boom".to_string(),
            }),
            creates_dir: None,
        };
        let lease_runner = lease_runner_with_setup(&workspace_path, "abc1234", vec![failing]);

        let error = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect_err("lease should surface setup failure");
        lease_runner.assert_exhausted();
        match error {
            CubeError::SetupStepFailed { step, error } => {
                assert_eq!(step, "deps");
                assert!(error.contains("pnpm"), "error mentions program: {error}");
            }
            other => panic!("unexpected error: {other:?}"),
        }

        // Lease is retained: the workspace row remains leased so the user
        // can rerun `cube workspace setup` to repair it.
        use crate::store::Store;
        let store = Store::open_at(&database_path).unwrap();
        let record = store
            .get_workspace_by_path(&workspace_path)
            .unwrap()
            .unwrap();
        assert_eq!(record.state, crate::metadata::WorkspaceState::Leased);
        assert!(record.lease_id.is_some());
    }

    #[test]
    fn workspace_lease_recovers_from_stale_jj_working_copy() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-004");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // First `jj git fetch` returns the stale-working-copy error.
        // The wrapper should run `jj workspace update-stale` once, then
        // retry the original command. The remainder of the lease then
        // proceeds normally.
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::stale(workspace_path.clone(), "jj", &["git", "fetch"]),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["workspace", "update-stale"],
                "Working copy now at: abc1234",
            ),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "stale demo"]),
            Some(&database_path),
            &runner,
        )
        .expect("lease should auto-recover from stale");
        runner.assert_exhausted();

        assert_eq!(
            result.payload["workspace"]["workspace_id"],
            "mono-agent-004"
        );
        assert_eq!(result.payload["workspace"]["head_commit"], "abc1234");

        // The recovery is observable in the audit log.
        let audit_dir = database_path.parent().unwrap().join("audit");
        let logs = std::fs::read_dir(&audit_dir)
            .expect("audit dir")
            .filter_map(|e| e.ok())
            .map(|e| std::fs::read_to_string(e.path()).expect("audit log"))
            .collect::<Vec<_>>()
            .join("");
        assert!(
            logs.contains("\"event\":\"workspace.stale_recovered\""),
            "expected stale_recovered audit event, got: {logs}"
        );
        assert!(
            logs.contains(workspace_path.display().to_string().as_str()),
            "audit event should record the workspace path"
        );
    }

    #[test]
    fn workspace_lease_surfaces_stale_recovery_failure() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-004");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // `jj git fetch` reports stale; `jj workspace update-stale`
        // itself fails. The lease must not pretend success — surface a
        // distinct StaleRecoveryFailed error.
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::stale(workspace_path.clone(), "jj", &["git", "fetch"]),
            ExpectedCommand {
                cwd: workspace_path.clone(),
                program: "jj".to_string(),
                args: vec!["workspace".to_string(), "update-stale".to_string()],
                result: Err(CubeError::CommandFailed {
                    program: "jj".to_string(),
                    args: vec!["workspace".to_string(), "update-stale".to_string()],
                    status: Some(1),
                    stderr: "Error: workspace operation failed".to_string(),
                }),
                creates_dir: None,
            },
        ]);

        let error = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "stale fail"]),
            Some(&database_path),
            &runner,
        )
        .expect_err("lease should fail when stale recovery itself fails");
        runner.assert_exhausted();

        match error {
            CubeError::StaleRecoveryFailed {
                workspace_path: path,
                cause,
            } => {
                assert_eq!(path, workspace_path);
                assert!(
                    cause.contains("update-stale"),
                    "cause should mention update-stale: {cause}"
                );
            }
            other => panic!("expected StaleRecoveryFailed, got {other:?}"),
        }
    }

    #[test]
    fn workspace_lease_recovers_from_op_log_divergence() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-004");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // `jj status` returns the op-log divergence error (exit 255,
        // "seems to be a sibling"). The wrapper should run
        // `jj workspace update-stale` once, then retry `jj status`. The
        // remainder of the lease then proceeds normally.
        let runner = FakeRunner::new(vec![
            ExpectedCommand::op_diverged(workspace_path.clone(), "jj", &["status", "--no-pager"]),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["workspace", "update-stale"],
                "Working copy now at: abc1234",
            ),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "op-diverged demo"]),
            Some(&database_path),
            &runner,
        )
        .expect("lease should auto-recover from op-log divergence");
        runner.assert_exhausted();

        assert_eq!(
            result.payload["workspace"]["workspace_id"],
            "mono-agent-004"
        );
        assert_eq!(result.payload["workspace"]["head_commit"], "abc1234");

        let audit_dir = database_path.parent().unwrap().join("audit");
        let logs = std::fs::read_dir(&audit_dir)
            .expect("audit dir")
            .filter_map(|e| e.ok())
            .map(|e| std::fs::read_to_string(e.path()).expect("audit log"))
            .collect::<Vec<_>>()
            .join("");
        assert!(
            logs.contains("\"event\":\"workspace.op_diverged_recovered\""),
            "expected op_diverged_recovered audit event, got: {logs}"
        );
        assert!(
            logs.contains(workspace_path.display().to_string().as_str()),
            "audit event should record the workspace path"
        );
    }

    #[test]
    fn workspace_lease_surfaces_op_diverged_recovery_failure() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-004");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // `jj status` reports op-log divergence; `jj workspace update-stale`
        // itself fails. The lease must surface a StaleRecoveryFailed error
        // with the original error preserved.
        let runner = FakeRunner::new(vec![
            ExpectedCommand::op_diverged(workspace_path.clone(), "jj", &["status", "--no-pager"]),
            ExpectedCommand {
                cwd: workspace_path.clone(),
                program: "jj".to_string(),
                args: vec!["workspace".to_string(), "update-stale".to_string()],
                result: Err(CubeError::CommandFailed {
                    program: "jj".to_string(),
                    args: vec!["workspace".to_string(), "update-stale".to_string()],
                    status: Some(1),
                    stderr: "Error: workspace operation failed".to_string(),
                }),
                creates_dir: None,
            },
        ]);

        let error = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "op-diverged fail"]),
            Some(&database_path),
            &runner,
        )
        .expect_err("lease should fail when op-diverged recovery itself fails");
        runner.assert_exhausted();

        match error {
            CubeError::StaleRecoveryFailed {
                workspace_path: path,
                cause,
            } => {
                assert_eq!(path, workspace_path);
                assert!(
                    cause.contains("update-stale"),
                    "cause should mention update-stale: {cause}"
                );
            }
            other => panic!("expected StaleRecoveryFailed, got {other:?}"),
        }
    }

    #[test]
    fn workspace_lease_colocate_inits_when_git_repo_has_no_jj() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-004");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");
        // Simulate a workspace that has .git but no .jj.
        std::fs::create_dir_all(workspace_path.join(".git")).expect(".git dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // `jj status` returns the "no jj repo" error. The wrapper should
        // run `jj git init --colocate` once, then retry `jj status`. The
        // remainder of the lease proceeds normally.
        let runner = FakeRunner::new(vec![
            ExpectedCommand::no_jj_repo(workspace_path.clone(), "jj", &["status", "--no-pager"]),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["git", "init", "--colocate"],
                "",
            ),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "colocate init demo"]),
            Some(&database_path),
            &runner,
        )
        .expect("lease should auto-recover by running jj git init --colocate");
        runner.assert_exhausted();

        assert_eq!(
            result.payload["workspace"]["workspace_id"],
            "mono-agent-004"
        );
        assert_eq!(result.payload["workspace"]["head_commit"], "abc1234");

        let audit_dir = database_path.parent().unwrap().join("audit");
        let logs = std::fs::read_dir(&audit_dir)
            .expect("audit dir")
            .filter_map(|e| e.ok())
            .map(|e| std::fs::read_to_string(e.path()).expect("audit log"))
            .collect::<Vec<_>>()
            .join("");
        assert!(
            logs.contains("\"event\":\"workspace.jj_colocate_initialised\""),
            "expected jj_colocate_initialised audit event, got: {logs}"
        );
        assert!(
            logs.contains(workspace_path.display().to_string().as_str()),
            "audit event should record the workspace path"
        );
    }

    #[test]
    fn workspace_lease_self_heals_broken_empty_and_auto_creates() {
        // A workspace directory with neither .jj/ nor .git/ is a husk holding
        // no recoverable work. Rather than blocking the lease, cube detects it
        // via a directory check (no jj `status` call on the husk), GCs it
        // (removes the directory and forgets its row), and provisions a fresh
        // workspace by cloning. The lease then succeeds (issue #845).
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let husk_path = workspace_root.join("mono-agent-004");
        // Intentionally no .jj/ or .git/ — this is the broken-empty state.
        std::fs::create_dir_all(&husk_path).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // After the husk is GC'd the pool is empty, so `next_workspace_id`
        // reuses the lowest slot. The runner expects only the clone + track +
        // reset sequence for the fresh workspace — never a `status` call
        // against the broken-empty husk.
        let new_path = workspace_root.join("mono-agent-001");
        let staging = workspace_root.join(".incoming-mono-agent-001");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "--colocate",
                    "git@github.com:spinyfin/mono.git",
                    &staging.display().to_string(),
                ],
                "",
            )
            .creating_dir(staging.clone()),
            ExpectedCommand::ok(staging.clone(), "jj", &["bookmark", "track", "main@origin"], ""),
            ExpectedCommand::no_such_remote_bookmark(
                staging.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
            ),
            ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                new_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "no git dir"]),
            Some(&database_path),
            &runner,
        )
        .expect("lease should self-heal the husk and auto-create a fresh workspace");
        runner.assert_exhausted();

        assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-001");
        assert_eq!(result.payload["workspace"]["state"], "leased");

        // The husk directory was removed and its registry row forgotten.
        assert!(!husk_path.exists(), "broken-empty husk should be removed");
        use crate::store::{Store, WorkspaceListFilter};
        let store = Store::open_at(&database_path).unwrap();
        let rows = store
            .list_workspaces_filtered(&WorkspaceListFilter::default())
            .unwrap();
        let ids: Vec<_> = rows.iter().map(|r| r.workspace_id.as_str()).collect();
        assert!(!ids.contains(&"mono-agent-004"), "husk row should be forgotten; saw {ids:?}");
        assert!(ids.contains(&"mono-agent-001"), "fresh workspace should exist; saw {ids:?}");

        // Audit log records both the detection and the GC of the husk.
        let events = audit_events(&tempdir);
        assert!(
            events.iter().any(|e| e["event"] == "workspace.broken_empty"),
            "expected workspace.broken_empty audit event; got: {events:?}"
        );
        assert!(
            events.iter().any(|e| e["event"] == "workspace.broken_empty_gc"
                && e["workspace_id"] == "mono-agent-004"),
            "expected workspace.broken_empty_gc audit event for the husk; got: {events:?}"
        );
    }

    #[test]
    fn workspace_lease_self_heals_two_broken_empty_husks() {
        // Exact repro from issue #845: every free workspace is broken-empty
        // (the `ci-infra-027` / `ci-infra-028` case). The lease must GC both
        // husks and provision a fresh workspace rather than failing with
        // "no free workspace".
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let husk_a = workspace_root.join("mono-agent-027");
        let husk_b = workspace_root.join("mono-agent-028");
        // Neither has .jj/ nor .git/ — both are broken-empty husks.
        std::fs::create_dir_all(&husk_a).expect("husk a");
        std::fs::create_dir_all(&husk_b).expect("husk b");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // Both husks GC'd → pool empty → fresh workspace takes the lowest slot.
        let new_path = workspace_root.join("mono-agent-001");
        let staging = workspace_root.join(".incoming-mono-agent-001");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "--colocate",
                    "git@github.com:spinyfin/mono.git",
                    &staging.display().to_string(),
                ],
                "",
            )
            .creating_dir(staging.clone()),
            ExpectedCommand::ok(staging.clone(), "jj", &["bookmark", "track", "main@origin"], ""),
            ExpectedCommand::no_such_remote_bookmark(
                staging.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
            ),
            ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                new_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "two husks"]),
            Some(&database_path),
            &runner,
        )
        .expect("lease should succeed by GC'ing both husks and auto-creating");
        runner.assert_exhausted();

        assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-001");
        assert!(!husk_a.exists(), "husk 027 should be removed");
        assert!(!husk_b.exists(), "husk 028 should be removed");
    }

    #[test]
    fn workspace_lease_gcs_broken_empty_and_keeps_dirty_then_auto_creates() {
        // Mixed pool: one dirty workspace (holds possibly-unpushed work) and
        // one broken-empty husk. The husk is GC'd and a fresh workspace is
        // auto-created; the dirty workspace is left untouched for the operator
        // to reclaim. A broken-empty entry must never turn into a hard stop,
        // even when a dirty entry is also present.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let dirty_path = workspace_root.join("mono-agent-003");
        let husk_path = workspace_root.join("mono-agent-027");
        std::fs::create_dir_all(dirty_path.join(".jj")).expect("dirty dir");
        std::fs::create_dir_all(&husk_path).expect("husk dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // Health check visits 003 (dirty `status`) then 027 (broken-empty, no
        // jj call). The husk is GC'd; `next_workspace_id` over the surviving
        // dirty 003 yields mono-agent-004 for the fresh clone.
        let new_path = workspace_root.join("mono-agent-004");
        let staging = workspace_root.join(".incoming-mono-agent-004");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(dirty_path.clone(), "jj", &["status", "--no-pager"], jj_status_dirty()),
            ExpectedCommand::ok(
                workspace_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "--colocate",
                    "git@github.com:spinyfin/mono.git",
                    &staging.display().to_string(),
                ],
                "",
            )
            .creating_dir(staging.clone()),
            ExpectedCommand::ok(staging.clone(), "jj", &["bookmark", "track", "main@origin"], ""),
            ExpectedCommand::no_such_remote_bookmark(
                staging.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
            ),
            ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                new_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "abc1234",
            ),
        ]);

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "dirty plus husk"]),
            Some(&database_path),
            &runner,
        )
        .expect("lease should succeed: GC the husk, keep the dirty one, auto-create");
        runner.assert_exhausted();

        assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-004");
        // The husk is gone; the dirty workspace is preserved for inspection.
        assert!(!husk_path.exists(), "broken-empty husk should be removed");
        assert!(dirty_path.exists(), "dirty workspace must be left untouched");
        use crate::store::Store;
        let store = Store::open_at(&database_path).unwrap();
        let dirty_row = store.get_workspace_by_path(&dirty_path).unwrap().unwrap();
        assert_eq!(
            dirty_row.health_status,
            Some(crate::metadata::WorkspaceHealth::Dirty),
            "dirty workspace should still be marked dirty"
        );
    }

    /// Sets `lease_expires_at_epoch_s` directly in the SQLite store.
    /// Used by reconcile tests to age a lease past its TTL without having
    /// to wait wall-clock seconds.
    fn force_lease_expiry(database_path: &std::path::Path, lease_id: &str, epoch_s: i64) {
        let conn = rusqlite::Connection::open(database_path).expect("sqlite open");
        let updated = conn
            .execute(
                "UPDATE workspaces SET lease_expires_at_epoch_s = ?1 WHERE lease_id = ?2",
                rusqlite::params![epoch_s, lease_id],
            )
            .expect("force expiry");
        assert_eq!(updated, 1, "expected exactly one row updated by force_lease_expiry");
    }

    fn audit_events(tempdir: &TempDir) -> Vec<serde_json::Value> {
        let audit_dir = tempdir.path().join("audit");
        let Ok(read) = std::fs::read_dir(&audit_dir) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        for entry in read.flatten() {
            let contents = std::fs::read_to_string(entry.path()).expect("audit content");
            for line in contents.lines() {
                events.push(serde_json::from_str(line).expect("audit line"));
            }
        }
        events
    }

    #[test]
    fn workspace_list_reconciles_free_row_whose_directory_is_missing() {
        // Canonical scenario from the chore: an operator wiped the
        // workspace directory by hand and the row remained in cube's
        // registry. `cube workspace list` must notice and self-heal
        // rather than handing out the stale row to the next caller.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-007");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // Seed a free row, then yank the directory out from under cube.
        {
            use crate::metadata::WorkspaceCandidate;
            use crate::store::Store;
            let mut store = Store::open_at(&database_path).unwrap();
            store
                .sync_workspaces(
                    "mono",
                    &[WorkspaceCandidate {
                        workspace_id: "mono-agent-007".to_string(),
                        workspace_path: workspace_path.clone(),
                    }],
                )
                .unwrap();
        }
        std::fs::remove_dir_all(&workspace_path).expect("wipe workspace dir");

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "list"]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("list");

        assert_eq!(
            result.payload["reconciled"]["removed"][0]["workspace_id"],
            "mono-agent-007"
        );
        assert_eq!(
            result.payload["reconciled"]["removed"][0]["prior_state"],
            "free"
        );
        assert_eq!(result.payload["reconciled"]["held"], json!([]));
        assert_eq!(result.payload["workspaces"], json!([]));

        use crate::store::{Store, WorkspaceListFilter};
        let store = Store::open_at(&database_path).unwrap();
        let remaining = store
            .list_workspaces_filtered(&WorkspaceListFilter::default())
            .unwrap();
        assert!(remaining.is_empty(), "row must be deleted by reconcile");

        let events = audit_events(&tempdir);
        let reconciled: Vec<_> = events
            .iter()
            .filter(|e| e["event"] == "workspace.dir_missing_reconciled")
            .collect();
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0]["repo"], "mono");
        assert_eq!(reconciled[0]["workspace_id"], "mono-agent-007");
        assert_eq!(reconciled[0]["prior_state"], "free");
    }

    #[test]
    fn workspace_list_reconciles_leased_row_with_expired_lease() {
        // A worker leased a workspace, then was rm-rf'd along with its
        // directory and never released. The lease has aged past its TTL,
        // so reconcile is allowed to force-release and forget the row.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let lease_runner = lease_runner_for(&workspace_path, "abc1234");
        let lease_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        let lease_id = lease_result.payload["workspace"]["lease_id"]
            .as_str()
            .expect("lease id")
            .to_string();
        lease_runner.assert_exhausted();

        // Age the lease into the past, then wipe the directory.
        force_lease_expiry(&database_path, &lease_id, 1);
        std::fs::remove_dir_all(&workspace_path).expect("wipe workspace dir");

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "list"]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("list");

        assert_eq!(
            result.payload["reconciled"]["removed"][0]["workspace_id"],
            "mono-agent-001"
        );
        assert_eq!(
            result.payload["reconciled"]["removed"][0]["prior_state"],
            "leased"
        );
        assert_eq!(result.payload["reconciled"]["held"], json!([]));

        use crate::store::{Store, WorkspaceListFilter};
        let store = Store::open_at(&database_path).unwrap();
        let remaining = store
            .list_workspaces_filtered(&WorkspaceListFilter::default())
            .unwrap();
        assert!(
            remaining.is_empty(),
            "expired+missing row must be force-released and deleted"
        );

        let events = audit_events(&tempdir);
        let reconciled: Vec<_> = events
            .iter()
            .filter(|e| e["event"] == "workspace.dir_missing_reconciled")
            .collect();
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0]["prior_state"], "leased");
        assert_eq!(reconciled[0]["lease_id"], lease_id);
    }

    #[test]
    fn workspace_list_holds_leased_row_when_lease_still_active() {
        // The lease is still within its TTL, so we can't know whether
        // the holder is mid-setup or genuinely dead. Defer to the
        // operator: warn + audit but leave the row untouched.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let lease_runner = lease_runner_for(&workspace_path, "abc1234");
        let lease_result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("lease");
        let lease_id = lease_result.payload["workspace"]["lease_id"]
            .as_str()
            .expect("lease id")
            .to_string();
        lease_runner.assert_exhausted();

        // Push the expiry far into the future so reconcile sees it as
        // active even after we wipe the directory.
        let far_future = current_epoch_s().expect("now") + 86_400;
        force_lease_expiry(&database_path, &lease_id, far_future);
        std::fs::remove_dir_all(&workspace_path).expect("wipe workspace dir");

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "list"]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("list");

        assert_eq!(result.payload["reconciled"]["removed"], json!([]));
        assert_eq!(
            result.payload["reconciled"]["held"][0]["workspace_id"],
            "mono-agent-001"
        );
        assert_eq!(
            result.payload["reconciled"]["held"][0]["prior_state"],
            "leased"
        );

        use crate::store::{Store, WorkspaceListFilter};
        let store = Store::open_at(&database_path).unwrap();
        let remaining = store
            .list_workspaces_filtered(&WorkspaceListFilter::default())
            .unwrap();
        assert_eq!(
            remaining.len(),
            1,
            "active-lease+missing row must be left in place"
        );
        assert_eq!(remaining[0].state, crate::metadata::WorkspaceState::Leased);

        let events = audit_events(&tempdir);
        let held: Vec<_> = events
            .iter()
            .filter(|e| e["event"] == "workspace.dir_missing_held")
            .collect();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0]["lease_id"], lease_id);
        assert_eq!(held[0]["lease_expires_at_epoch_s"], far_future);
    }

    #[test]
    fn workspace_list_reconcile_is_noop_when_directories_exist() {
        // When nothing has drifted, reconcile must not emit any audit
        // events or surface any reconciled/held rows.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        {
            use crate::metadata::WorkspaceCandidate;
            use crate::store::Store;
            let mut store = Store::open_at(&database_path).unwrap();
            store
                .sync_workspaces(
                    "mono",
                    &[WorkspaceCandidate {
                        workspace_id: "mono-agent-001".to_string(),
                        workspace_path: workspace_path.clone(),
                    }],
                )
                .unwrap();
        }

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "list"]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("list");

        assert_eq!(result.payload["reconciled"]["removed"], json!([]));
        assert_eq!(result.payload["reconciled"]["held"], json!([]));
        assert!(audit_events(&tempdir).is_empty());
    }

    #[test]
    fn workspace_list_reconciler_respects_repo_filter() {
        // With --repo set, only that repo's drifted rows should be
        // reconciled. Other repos' dangling rows must be left alone so a
        // narrow query doesn't quietly mutate state across the registry.
        let (tempdir, database_path) = with_database_path();
        let workspace_root_a = tempdir.path().join("repos-a/workspaces");
        let workspace_root_b = tempdir.path().join("repos-b/workspaces");
        std::fs::create_dir_all(workspace_root_a.join("mono-agent-001").join(".jj"))
            .expect("workspace dir a");
        std::fs::create_dir_all(workspace_root_b.join("other-agent-001").join(".jj"))
            .expect("workspace dir b");

        run_with_dependencies(
            Cli::parse_from([
                "cube",
                "repo",
                "add",
                "mono",
                "--origin",
                "git@github.com:spinyfin/mono.git",
                "--workspace-root",
                &workspace_root_a.display().to_string(),
                "--workspace-prefix",
                "mono-agent-",
            ]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo a");
        run_with_dependencies(
            Cli::parse_from([
                "cube",
                "repo",
                "add",
                "other",
                "--origin",
                "git@github.com:spinyfin/other.git",
                "--workspace-root",
                &workspace_root_b.display().to_string(),
                "--workspace-prefix",
                "other-agent-",
            ]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo b");

        // Seed both repos with one free row each, then wipe both dirs.
        {
            use crate::metadata::WorkspaceCandidate;
            use crate::store::Store;
            let mut store = Store::open_at(&database_path).unwrap();
            store
                .sync_workspaces(
                    "mono",
                    &[WorkspaceCandidate {
                        workspace_id: "mono-agent-001".to_string(),
                        workspace_path: workspace_root_a.join("mono-agent-001"),
                    }],
                )
                .unwrap();
            store
                .sync_workspaces(
                    "other",
                    &[WorkspaceCandidate {
                        workspace_id: "other-agent-001".to_string(),
                        workspace_path: workspace_root_b.join("other-agent-001"),
                    }],
                )
                .unwrap();
        }
        std::fs::remove_dir_all(workspace_root_a.join("mono-agent-001"))
            .expect("wipe a");
        std::fs::remove_dir_all(workspace_root_b.join("other-agent-001"))
            .expect("wipe b");

        let result = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "list", "--repo", "mono"]),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("list");

        // Only the `mono` row should appear in the reconcile report.
        let removed = result.payload["reconciled"]["removed"]
            .as_array()
            .unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0]["repo"], "mono");

        // The `other` repo's dangling row must still be there.
        use crate::store::{Store, WorkspaceListFilter};
        let store = Store::open_at(&database_path).unwrap();
        let other = store
            .list_workspaces_filtered(&WorkspaceListFilter {
                repo: Some("other"),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].workspace_id, "other-agent-001");
    }

    #[test]
    fn workspace_lease_reconciles_expired_missing_row_before_claiming() {
        // A previously leased workspace's directory was wiped while the
        // lease aged out. Lease must reconcile the dangling row before
        // claiming so it doesn't hand out the stale slot.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let lease_runner = lease_runner_for(&workspace_path, "abc1234");
        let first = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("first lease");
        let lease_id = first.payload["workspace"]["lease_id"]
            .as_str()
            .expect("lease id")
            .to_string();
        lease_runner.assert_exhausted();

        force_lease_expiry(&database_path, &lease_id, 1);
        std::fs::remove_dir_all(&workspace_path).expect("wipe workspace dir");

        // The next lease should reconcile the phantom row, then auto-create
        // a fresh workspace via `jj git clone --colocate`. The runner needs
        // the clone command plus the standard reset/log triple for the
        // newly-created workspace. After reconcile deletes mono-agent-001,
        // `next_workspace_id` reuses the freed slot rather than skipping
        // ahead to mono-agent-002.
        let new_path = workspace_root.join("mono-agent-001");
        let staging = workspace_root.join(".incoming-mono-agent-001");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "--colocate",
                    "git@github.com:spinyfin/mono.git",
                    &staging.display().to_string(),
                ],
                "",
            )
            .creating_dir(staging.clone()),
            ExpectedCommand::ok(
                staging.clone(),
                "jj",
                &["bookmark", "track", "main@origin"],
                "",
            ),
            ExpectedCommand::no_such_remote_bookmark(
                staging.clone(),
                "jj",
                &["bookmark", "track", "master@origin"],
            ),
            ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(new_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                new_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "def5678",
            ),
        ]);

        let second = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "fresh"]),
            Some(&database_path),
            &runner,
        )
        .expect("second lease");
        runner.assert_exhausted();

        assert_eq!(
            second.payload["workspace"]["workspace_id"],
            "mono-agent-001"
        );

        // Only the freshly-claimed (re-provisioned) row remains; the
        // phantom row was forgotten before the new clone created the
        // replacement.
        use crate::store::{Store, WorkspaceListFilter};
        let store = Store::open_at(&database_path).unwrap();
        let rows = store
            .list_workspaces_filtered(&WorkspaceListFilter::default())
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].workspace_id, "mono-agent-001");
        assert_eq!(rows[0].state, crate::metadata::WorkspaceState::Leased);

        let events = audit_events(&tempdir);
        let reconciled: Vec<_> = events
            .iter()
            .filter(|e| {
                e["event"] == "workspace.dir_missing_reconciled"
                    && e["workspace_id"] == "mono-agent-001"
            })
            .collect();
        assert_eq!(reconciled.len(), 1);
    }

    /// Regression for the 2026-05-12 "`@` got re-pointed mid-flight"
    /// incident. Setup mimics the race exactly:
    ///   1. A worker leases a workspace and starts editing — `@` ends
    ///      up off main on an unbookmarked change (the worker's WIP).
    ///   2. The worker's lease ages past its TTL (engine forgot to
    ///      heartbeat — the orthogonal bug the engine-side fix
    ///      addresses).
    ///   3. A new lease request arrives. The expected old behavior was
    ///      to silently `expire_stale_leases`, claim the slot, and run
    ///      `jj new <main>` — moving the still-active worker's `@`.
    ///
    /// The fix this test pins down: cube's reset path now checks `@`'s
    /// emptiness and parent-bookmark before running `jj new`, sees the
    /// workspace is still on a non-main change with content, refuses
    /// to reset, releases the just-acquired lease, and surfaces
    /// `LeaseExpiredWorkspaceDirty` so the caller can fail loudly
    /// instead of clobbering the prior worker's work.
    #[test]
    fn second_lease_refuses_to_reset_workspace_with_uncommitted_prior_work() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // First lease — normal happy path.
        let lease_runner = lease_runner_for(&workspace_path, "abc1234");
        let first = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "wip"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("first lease");
        let prior_lease_id = first.payload["workspace"]["lease_id"]
            .as_str()
            .expect("lease id")
            .to_string();
        lease_runner.assert_exhausted();

        // Worker has been editing — `@` is off main, has uncommitted
        // content. Force expiry so `expire_stale_leases` reclaims it
        // on the next lease call.
        force_lease_expiry(&database_path, &prior_lease_id, 1);

        // The second lease's reset path should run `jj status --no-pager`
        // (health check), then `jj git fetch`, then the head-status probe
        // and stop. Stub the probe to return a non-empty `@` whose parent
        // isn't `main` — exactly the shape a still-active worker's WIP looks like.
        let probe_output = "abcd1234\tfalse\tfeature-bookmark";
        let second_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &[
                    "log",
                    "--no-graph",
                    "-r",
                    "@",
                    "-T",
                    "change_id ++ \"\\t\" ++ empty ++ \"\\t\" ++ parents.map(|p| p.bookmarks().join(\",\")).join(\";\")",
                ],
                probe_output,
            ),
        ]);

        let err = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "incoming"]),
            Some(&database_path),
            &second_runner,
        )
        .expect_err("second lease must refuse to clobber the WIP");

        match &err {
            CubeError::LeaseExpiredWorkspaceDirty {
                workspace_path: refused_path,
                prior_lease_id: refused_lease,
                ..
            } => {
                assert_eq!(refused_path, &workspace_path);
                assert_eq!(refused_lease, &prior_lease_id);
            }
            other => panic!("expected LeaseExpiredWorkspaceDirty, got {other:?}"),
        }
        second_runner.assert_exhausted();

        // The crucial regression-pin: `jj new main` was NEVER invoked
        // on the leased workspace, so the active worker's `@` is
        // untouched. The probe is the only post-fetch jj call that
        // ran; the runner's exhausted assertion above proves no other
        // command was issued.
        let events = audit_events(&tempdir);
        let refused: Vec<_> = events
            .iter()
            .filter(|e| e["event"] == "workspace.reset_refused_dirty")
            .collect();
        assert_eq!(refused.len(), 1, "expected one workspace.reset_refused_dirty event");
        assert_eq!(refused[0]["prior_lease_id"], prior_lease_id);
        assert_eq!(refused[0]["workspace_path"], workspace_path.display().to_string());

        // `lease.expired_reclaimed` must also have been audited so the
        // timeline reads end-to-end ("we swept this lease, then we
        // refused to destructively reset its workspace").
        let reclaimed: Vec<_> = events
            .iter()
            .filter(|e| e["event"] == "lease.expired_reclaimed")
            .collect();
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0]["prior_lease_id"], prior_lease_id);

        // The new lease was rolled back: workspace is back to `free`
        // with `lease_setup_failed` recorded. The old (expired) row's
        // lease_id is gone — `expire_stale_leases` cleared it before
        // the refused claim.
        use crate::store::{Store, WorkspaceListFilter};
        let store = Store::open_at(&database_path).unwrap();
        let rows = store
            .list_workspaces_filtered(&WorkspaceListFilter::default())
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, crate::metadata::WorkspaceState::Free);
        assert_eq!(rows[0].last_release_reason.as_deref(), Some("lease_setup_failed"));
    }

    /// The dirty guard must NOT fire on the steady-state happy path:
    /// when `@` is empty and its parent is on `main`, the workspace is
    /// safe to reset and lease acquisition proceeds normally.
    #[test]
    fn second_lease_resets_normally_when_at_is_clean_on_main() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        let lease_runner = lease_runner_for(&workspace_path, "abc1234");
        let first = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "wip"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("first lease");
        let prior_lease_id = first.payload["workspace"]["lease_id"]
            .as_str()
            .expect("lease id")
            .to_string();
        lease_runner.assert_exhausted();

        force_lease_expiry(&database_path, &prior_lease_id, 1);

        // Clean @: empty, parent on main → safe to reset.
        let probe_output = "abcd1234\ttrue\tmain";
        let second_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &[
                    "log",
                    "--no-graph",
                    "-r",
                    "@",
                    "-T",
                    "change_id ++ \"\\t\" ++ empty ++ \"\\t\" ++ parents.map(|p| p.bookmarks().join(\",\")).join(\";\")",
                ],
                probe_output,
            ),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "def5678",
            ),
        ]);

        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "fresh"]),
            Some(&database_path),
            &second_runner,
        )
        .expect("second lease must succeed when the workspace is clean on main");
        second_runner.assert_exhausted();
    }

    /// When `--resume-pr` is combined with a workspace reclaimed from an expired
    /// lease, the dirty guard must fire and surface `LeaseExpiredWorkspaceDirty`
    /// instead of snapshotting the prior holder's uncommitted files into the new
    /// commit on top of the PR head.
    #[test]
    fn resume_pr_on_expired_lease_refuses_dirty_workspace() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // First lease — normal happy path.
        let lease_runner = lease_runner_for(&workspace_path, "abc1234");
        let first = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "wip"]),
            Some(&database_path),
            &lease_runner,
        )
        .expect("first lease");
        let prior_lease_id = first.payload["workspace"]["lease_id"]
            .as_str()
            .expect("lease id")
            .to_string();
        lease_runner.assert_exhausted();

        // Force the first lease to appear expired so the next lease call reclaims it.
        force_lease_expiry(&database_path, &prior_lease_id, 1);

        // The second lease uses --resume-pr. After the health check, it resolves
        // the github remote, fetches, then runs the head-status probe. The probe
        // returns a non-empty @ whose parent isn't main — exactly the WIP shape
        // left by a still-active prior worker. The guard must stop here and NOT
        // proceed to `gh pr view` or `jj new pr/<n>`.
        let github_remote = "github";
        let remote_list = format!("origin\t/local/mirror\n{github_remote}\tgit@github.com:spinyfin/mono.git\n");
        let probe_output = "abcd1234\tfalse\tfeature-bookmark";
        let head_status_template = "change_id ++ \"\\t\" ++ empty ++ \"\\t\" ++ parents.map(|p| p.bookmarks().join(\",\")).join(\";\")";
        let second_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["status", "--no-pager"], "The working copy is clean"),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "remote", "list"], &remote_list),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch", "--remote", github_remote], ""),
            ExpectedCommand::ok(
                workspace_path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", head_status_template],
                probe_output,
            ),
        ]);

        let err = run_with_dependencies(
            Cli::parse_from([
                "cube", "workspace", "lease", "mono",
                "--task", "resume dirty PR",
                "--resume-pr", "42",
            ]),
            Some(&database_path),
            &second_runner,
        )
        .expect_err("resume-pr on dirty expired workspace must refuse");

        match &err {
            CubeError::LeaseExpiredWorkspaceDirty {
                workspace_path: refused_path,
                prior_lease_id: refused_lease,
                ..
            } => {
                assert_eq!(refused_path, &workspace_path);
                assert_eq!(refused_lease, &prior_lease_id);
            }
            other => panic!("expected LeaseExpiredWorkspaceDirty, got {other:?}"),
        }
        second_runner.assert_exhausted();

        // The workspace is back to `free` — the new lease was rolled back.
        use crate::store::{Store, WorkspaceListFilter};
        let store = Store::open_at(&database_path).unwrap();
        let rows = store
            .list_workspaces_filtered(&WorkspaceListFilter::default())
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, crate::metadata::WorkspaceState::Free);
        assert_eq!(rows[0].last_release_reason.as_deref(), Some("lease_setup_failed"));
    }

    #[test]
    fn run_jj_propagates_non_stale_errors_unchanged() {
        // Non-stale jj failures must not trigger recovery — only the
        // specific stale signature is treated as recoverable.
        use crate::command_runner::CommandInvocation;
        let runner = FakeRunner::new(vec![ExpectedCommand {
            cwd: PathBuf::from("/tmp/ws"),
            program: "jj".to_string(),
            args: vec!["status".to_string()],
            result: Err(CubeError::CommandFailed {
                program: "jj".to_string(),
                args: vec!["status".to_string()],
                status: Some(1),
                stderr: "Error: something else entirely".to_string(),
            }),
            creates_dir: None,
        }]);

        let invocation = CommandInvocation {
            cwd: PathBuf::from("/tmp/ws"),
            program: "jj".to_string(),
            args: vec!["status".to_string()],
        };
        let err = super::run_jj(&runner, None, &invocation)
            .expect_err("non-stale failure should propagate");
        runner.assert_exhausted();
        assert!(
            matches!(err, CubeError::CommandFailed { .. }),
            "expected CommandFailed, got {err:?}"
        );
    }

    #[derive(Default)]
    struct FakeRunner {
        expectations: RefCell<VecDeque<ExpectedCommand>>,
    }

    impl FakeRunner {
        fn new(expectations: Vec<ExpectedCommand>) -> Self {
            Self {
                expectations: RefCell::new(expectations.into()),
            }
        }

        fn assert_exhausted(&self) {
            assert!(
                self.expectations.borrow().is_empty(),
                "unexpected commands remaining: {:?}",
                self.expectations.borrow()
            );
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, invocation: &CommandInvocation) -> Result<String> {
            let expected = self
                .expectations
                .borrow_mut()
                .pop_front()
                .expect("unexpected command invocation");
            assert_eq!(expected.cwd, invocation.cwd);
            assert_eq!(expected.program, invocation.program);
            assert_eq!(expected.args, invocation.args);
            if let Some(path) = &expected.creates_dir {
                std::fs::create_dir_all(path).expect("create simulated workspace dir");
            }
            expected.result
        }
    }

    #[derive(Debug)]
    struct ExpectedCommand {
        cwd: PathBuf,
        program: String,
        args: Vec<String>,
        result: Result<String>,
        creates_dir: Option<PathBuf>,
    }

    impl ExpectedCommand {
        fn ok(cwd: PathBuf, program: &str, args: &[&str], stdout: &str) -> Self {
            Self {
                cwd,
                program: program.to_string(),
                args: args.iter().map(|arg| (*arg).to_string()).collect(),
                result: Ok(stdout.to_string()),
                creates_dir: None,
            }
        }

        fn creating_dir(mut self, path: PathBuf) -> Self {
            self.creates_dir = Some(path);
            self
        }

        /// Build an expectation for the `git ls-remote --symref <origin> HEAD`
        /// default-branch probe, returning the symref output that points `HEAD`
        /// at `branch` (the shape `git` actually prints).
        fn ls_remote_symref(cwd: PathBuf, origin: &str, branch: &str) -> Self {
            Self::ok(
                cwd,
                "git",
                &["ls-remote", "--symref", origin, "HEAD"],
                &format!(
                    "ref: refs/heads/{branch}\tHEAD\n\
                     0000000000000000000000000000000000000000\tHEAD"
                ),
            )
        }

        /// Build an expectation that simulates jj's stale-working-copy
        /// failure. The wording matches what `cube`'s `run_jj` wrapper
        /// looks for via `JJ_STALE_SIGNATURE`.
        fn stale(cwd: PathBuf, program: &str, args: &[&str]) -> Self {
            let args_owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
            Self {
                cwd,
                program: program.to_string(),
                args: args_owned.clone(),
                result: Err(CubeError::CommandFailed {
                    program: program.to_string(),
                    args: args_owned,
                    status: Some(1),
                    stderr: "Error: The working copy is stale (not updated since operation \
                             0123456789ab). Run `jj workspace update-stale` to update it."
                        .to_string(),
                }),
                creates_dir: None,
            }
        }

        /// Build an expectation that simulates jj's op-log divergence
        /// failure. The wording matches `JJ_OP_DIVERGED_SIGNATURE` and
        /// is recovered by `jj workspace update-stale`, same as `stale`.
        fn op_diverged(cwd: PathBuf, program: &str, args: &[&str]) -> Self {
            let args_owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
            Self {
                cwd,
                program: program.to_string(),
                args: args_owned.clone(),
                result: Err(CubeError::CommandFailed {
                    program: program.to_string(),
                    args: args_owned,
                    status: Some(255),
                    stderr: "Internal error: The repo was loaded at operation a44a2f689f46, \
                             which seems to be a sibling of the working copy's operation \
                             17fb914fb03f"
                        .to_string(),
                }),
                creates_dir: None,
            }
        }

        /// Build an expectation that simulates `jj bookmark track
        /// <name>@origin` failing because the named remote bookmark
        /// does not exist in the repo. Matches `JJ_NO_REMOTE_BOOKMARK_SIGNATURE`
        /// and is the expected outcome for whichever of `main@origin` /
        /// `master@origin` this repo does not use.
        fn no_such_remote_bookmark(cwd: PathBuf, program: &str, args: &[&str]) -> Self {
            let args_owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
            let bookmark = args_owned.last().cloned().unwrap_or_default();
            Self {
                cwd,
                program: program.to_string(),
                args: args_owned.clone(),
                result: Err(CubeError::CommandFailed {
                    program: program.to_string(),
                    args: args_owned,
                    status: Some(1),
                    stderr: format!("Error: No such remote bookmark: {bookmark}"),
                }),
                creates_dir: None,
            }
        }

        /// Build an expectation that simulates `jj bookmark set <name> -r
        /// <name>@origin` failing because the remote-tracking target does
        /// not resolve. Matches `JJ_REVISION_DOESNT_EXIST_SIGNATURE` and is
        /// the wording jj prints when a repo's recorded default branch has
        /// no matching `@origin` bookmark — tolerated by the on-lease
        /// fast-forward.
        fn revision_doesnt_exist(cwd: PathBuf, program: &str, args: &[&str]) -> Self {
            let args_owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
            let target = args_owned
                .iter()
                .rev()
                .find(|a| a.contains("@origin"))
                .cloned()
                .unwrap_or_default();
            Self {
                cwd,
                program: program.to_string(),
                args: args_owned.clone(),
                result: Err(CubeError::CommandFailed {
                    program: program.to_string(),
                    args: args_owned,
                    status: Some(1),
                    stderr: format!("Error: Revision `{target}` doesn't exist"),
                }),
                creates_dir: None,
            }
        }

        /// Build an expectation that simulates jj's "no jj repo" failure.
        /// The wording matches `JJ_NO_JJ_REPO_SIGNATURE` and is recovered
        /// by `jj git init --colocate` when `.git/` is present.
        fn no_jj_repo(cwd: PathBuf, program: &str, args: &[&str]) -> Self {
            let args_owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
            Self {
                cwd,
                program: program.to_string(),
                args: args_owned.clone(),
                result: Err(CubeError::CommandFailed {
                    program: program.to_string(),
                    args: args_owned,
                    status: Some(1),
                    stderr: "Error: There is no jj repo in \".\"\n\
                             Hint: It looks like this is a git repo. You can create a jj repo \
                             backed by it by running this:\njj git init --colocate"
                        .to_string(),
                }),
                creates_dir: None,
            }
        }
    }

    #[test]
    fn workspace_dir_create_error_has_specific_variant() {
        // Ensure that when workspace directory creation fails, the error surfaces
        // as WorkspaceDirCreate (not the generic Io variant). This guards against
        // regressions to the old #[from] io::Error pattern that reported every
        // io error as "failed to prepare Cube data directory".
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");

        // Create a *file* at the workspace_root path so create_dir_all fails.
        std::fs::write(&workspace_root, b"not a dir").expect("write sentinel file");

        let defaults = RepoEnsureDefaults {
            repo_root: tempdir.path().join("repos"),
            workspace_root: workspace_root.clone(),
        };

        let cli = Cli::parse_from(["cube", "repo", "ensure", "--origin", "https://github.com/example/repo"]);
        let runner = crate::command_runner::RealCommandRunner;
        let err = run_with_context(cli, Some(&database_path), &runner, Some(&defaults), None)
            .expect_err("should fail because workspace_root is a file");

        assert!(
            matches!(err, CubeError::WorkspaceDirCreate { ref path, .. } if path == &workspace_root),
            "expected WorkspaceDirCreate, got: {err:?}"
        );
    }

    // --- resolve_body_file / stdin materialization tests ---

    #[test]
    fn is_stdin_path_recognises_known_aliases() {
        assert!(is_stdin_path("/dev/stdin"));
        assert!(is_stdin_path("-"));
        assert!(is_stdin_path("/dev/fd/0"));
    }

    #[test]
    fn is_stdin_path_does_not_match_regular_paths() {
        assert!(!is_stdin_path("/tmp/pr-body.md"));
        assert!(!is_stdin_path("/dev/null"));
        assert!(!is_stdin_path(""));
    }

    #[test]
    fn resolve_body_file_errors_on_empty_regular_file() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        // File is created empty by default.
        let result = resolve_body_file(&tmp.path().display().to_string());
        assert!(
            result.is_err(),
            "should error on empty file, got {:?}",
            result
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("empty"),
            "error should mention 'empty': {msg}"
        );
    }

    #[test]
    fn resolve_body_file_passes_through_non_empty_regular_file() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), b"## Summary\n\nBody text.").expect("write");
        let path_str = tmp.path().display().to_string();

        let (resolved, tmpfile) = resolve_body_file(&path_str).expect("resolve regular file");

        // Regular file: path unchanged, no temp file created.
        assert_eq!(resolved, path_str);
        assert!(tmpfile.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_body_file_materialises_fifo_content_to_temp_file() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("tempdir");
        let fifo_path = dir.path().join("test.fifo");

        // Create a FIFO.
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .expect("mkfifo");
        assert!(status.success(), "mkfifo failed");

        let expected_body = "## PR Body\n\nThis is the materialized body content.";
        let fifo_path_clone = fifo_path.clone();
        let body_clone = expected_body.to_string();

        // Write in a background thread — FIFO open blocks until a reader also opens.
        let writer = std::thread::spawn(move || {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(&fifo_path_clone)
                .expect("open fifo for write");
            f.write_all(body_clone.as_bytes()).expect("write fifo");
        });

        let path_str = fifo_path.display().to_string();
        let (resolved, tmp) = resolve_body_file(&path_str).expect("resolve fifo");

        writer.join().expect("writer thread");

        // resolved path must differ from the FIFO (temp file was created).
        assert_ne!(
            resolved, path_str,
            "resolved path should be a temp file, not the FIFO"
        );
        let materialized = std::fs::read_to_string(&resolved).expect("read materialized");
        assert_eq!(materialized, expected_body);

        if let Some(p) = tmp {
            let _ = std::fs::remove_file(p);
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolve_body_file_errors_on_empty_fifo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fifo_path = dir.path().join("empty.fifo");

        let status = std::process::Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .expect("mkfifo");
        assert!(status.success(), "mkfifo failed");

        let fifo_path_clone = fifo_path.clone();
        // Write empty content to FIFO so the reader gets EOF immediately.
        let writer = std::thread::spawn(move || {
            // Just open and close without writing.
            let _f = std::fs::OpenOptions::new()
                .write(true)
                .open(&fifo_path_clone)
                .expect("open fifo for write");
        });

        let path_str = fifo_path.display().to_string();
        let result = resolve_body_file(&path_str);

        writer.join().expect("writer thread");

        assert!(result.is_err(), "should error on empty FIFO");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("empty"), "error should mention 'empty': {msg}");
    }

    // --- ensure_pr body-file regression tests ---

    #[test]
    fn ensure_pr_uses_body_file_flag_not_body_flag() {
        // Regression: when --body-file is given, gh pr create must receive
        // --body-file <path>, NOT --body <content>. Passing the body inline
        // via --body "..." lets the shell evaluate backticks and $(...) before
        // cube ever sees the argument, corrupting PR bodies that contain
        // inline code.
        let body_content =
            "Use `rustc --help` or `$(cargo --version)` or ${CARGO_HOME}.\n\nMore `code`.";
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), body_content).expect("write body");
        let body_path = tmp.path().display().to_string();

        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["git", "remote", "list"],
                "origin\tgit@github.com:spinyfin/mono.git\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &[
                    "git", "push", "-b", "my-feature", "--remote", "origin", "--allow-new",
                ],
                "",
            ),
            // Push verification: local commit vs GitHub branch head sha.
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
                "abc123\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "api",
                    "repos/spinyfin/mono/branches/my-feature",
                    "--jq",
                    ".commit.sha",
                ],
                "abc123\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "pr", "list", "-R", "spinyfin/mono", "--head", "my-feature", "--state",
                    "open", "--json", "url",
                ],
                "[]",
            ),
            // The critical assertion: gh receives --body-file <path>, not --body <content>.
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "pr",
                    "create",
                    "-R",
                    "spinyfin/mono",
                    "--head",
                    "my-feature",
                    "--base",
                    "main",
                    "--title",
                    "Test PR",
                    "--body-file",
                    &body_path,
                ],
                "https://github.com/spinyfin/mono/pull/99",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["bookmark", "set", "pr/99", "-r", "my-feature"],
                "",
            ),
        ]);

        let cli = Cli::parse_from([
            "cube",
            "pr",
            "ensure",
            "--branch",
            "my-feature",
            "--title",
            "Test PR",
            "--body-file",
            &body_path,
        ]);
        let result =
            run_with_dependencies(cli, None, &runner).expect("ensure_pr with --body-file");
        runner.assert_exhausted();

        assert_eq!(result.payload["url"], "https://github.com/spinyfin/mono/pull/99");
        // Body file must not be modified — backticks and $(...) survive verbatim.
        let body_on_disk = std::fs::read_to_string(tmp.path()).expect("read body");
        assert_eq!(body_on_disk, body_content);
    }

    // --- ensure_pr JSON output shape tests ---

    #[test]
    fn ensure_pr_created_json_has_action_url_number() {
        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["git", "remote", "list"],
                "origin\tgit@github.com:spinyfin/mono.git\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &[
                    "git", "push", "-b", "my-feature", "--remote", "origin", "--allow-new",
                ],
                "",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
                "abc123\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "api",
                    "repos/spinyfin/mono/branches/my-feature",
                    "--jq",
                    ".commit.sha",
                ],
                "abc123\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "pr", "list", "-R", "spinyfin/mono", "--head", "my-feature", "--state",
                    "open", "--json", "url",
                ],
                "[]",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "pr", "create", "-R", "spinyfin/mono", "--head", "my-feature", "--base",
                    "main", "--title", "New PR",
                ],
                "https://github.com/spinyfin/mono/pull/42",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["bookmark", "set", "pr/42", "-r", "my-feature"],
                "",
            ),
        ]);

        let cli = Cli::parse_from([
            "cube", "pr", "ensure", "--branch", "my-feature", "--title", "New PR",
        ]);
        let result = run_with_dependencies(cli, None, &runner).expect("ensure_pr created");
        runner.assert_exhausted();

        assert_eq!(result.message, "https://github.com/spinyfin/mono/pull/42");
        assert_eq!(result.payload["action"], "created");
        assert_eq!(result.payload["url"], "https://github.com/spinyfin/mono/pull/42");
        assert_eq!(result.payload["number"], 42);
        assert_eq!(result.payload["pr_bookmark"], "pr/42");
    }

    #[test]
    fn ensure_pr_exists_json_has_action_url_number() {
        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["git", "remote", "list"],
                "origin\tgit@github.com:spinyfin/mono.git\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &[
                    "git", "push", "-b", "my-feature", "--remote", "origin", "--allow-new",
                ],
                "",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
                "abc123\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "api",
                    "repos/spinyfin/mono/branches/my-feature",
                    "--jq",
                    ".commit.sha",
                ],
                "abc123\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "pr", "list", "-R", "spinyfin/mono", "--head", "my-feature", "--state",
                    "open", "--json", "url",
                ],
                r#"[{"url":"https://github.com/spinyfin/mono/pull/7"}]"#,
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["bookmark", "set", "pr/7", "-r", "my-feature"],
                "",
            ),
        ]);

        let cli = Cli::parse_from([
            "cube", "pr", "ensure", "--branch", "my-feature", "--title", "Existing PR",
        ]);
        let result = run_with_dependencies(cli, None, &runner).expect("ensure_pr exists");
        runner.assert_exhausted();

        assert_eq!(result.message, "https://github.com/spinyfin/mono/pull/7");
        assert_eq!(result.payload["action"], "exists");
        assert_eq!(result.payload["url"], "https://github.com/spinyfin/mono/pull/7");
        assert_eq!(result.payload["number"], 7);
        assert_eq!(result.payload["pr_bookmark"], "pr/7");
    }

    #[test]
    fn ensure_pr_pushes_to_github_remote_not_local_mirror() {
        // Regression for the "false push" trap: a cube workspace has a local
        // mirror named `origin` and the real GitHub upstream named `github`.
        // `cube pr ensure` must push to `github` (resolved by URL, not by the
        // conventional `origin` name) and verify the push against GitHub.
        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["git", "remote", "list"],
                "origin\t/Users/bduff/dev/agents/repos/mono\n\
                 github\tgit@github.com:spinyfin/mono.git\n",
            ),
            // Push targets `github`, the real upstream — NOT the local mirror.
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &[
                    "git", "push", "-b", "my-feature", "--remote", "github", "--allow-new",
                ],
                "",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
                "deadbeef\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "api",
                    "repos/spinyfin/mono/branches/my-feature",
                    "--jq",
                    ".commit.sha",
                ],
                "deadbeef\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "pr", "list", "-R", "spinyfin/mono", "--head", "my-feature", "--state",
                    "open", "--json", "url",
                ],
                "[]",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "pr", "create", "-R", "spinyfin/mono", "--head", "my-feature", "--base",
                    "main", "--title", "New PR",
                ],
                "https://github.com/spinyfin/mono/pull/77",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["bookmark", "set", "pr/77", "-r", "my-feature"],
                "",
            ),
        ]);

        let cli = Cli::parse_from([
            "cube", "pr", "ensure", "--branch", "my-feature", "--title", "New PR",
        ]);
        let result = run_with_dependencies(cli, None, &runner).expect("ensure_pr created");
        runner.assert_exhausted();
        assert_eq!(result.payload["url"], "https://github.com/spinyfin/mono/pull/77");
    }

    #[test]
    fn ensure_pr_fails_loudly_when_push_did_not_reach_github() {
        // The push "succeeded" locally but GitHub's branch head sha does not
        // match the local commit — the classic local-mirror false positive.
        // ensure_pr must error rather than report a stale PR as updated.
        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["git", "remote", "list"],
                "origin\tgit@github.com:spinyfin/mono.git\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &[
                    "git", "push", "-b", "my-feature", "--remote", "origin", "--allow-new",
                ],
                "",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
                "4ce6198\n",
            ),
            // GitHub still has the OLD sha — push never reached it.
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "api",
                    "repos/spinyfin/mono/branches/my-feature",
                    "--jq",
                    ".commit.sha",
                ],
                "2f8dd09\n",
            ),
        ]);

        let cli = Cli::parse_from([
            "cube", "pr", "ensure", "--branch", "my-feature", "--title", "New PR",
        ]);
        let err = run_with_dependencies(cli, None, &runner)
            .expect_err("ensure_pr should fail when push did not reach GitHub");
        runner.assert_exhausted();
        let msg = err.to_string();
        assert!(
            msg.contains("push verification failed") && msg.contains("4ce6198") && msg.contains("2f8dd09"),
            "error should name the mismatch loudly: {msg}"
        );
    }

    #[test]
    fn ensure_pr_guard_rejects_pr_bookmark_branch_arg() {
        // Passing --branch pr/42 to `cube pr ensure` must be refused before any
        // push is attempted — the `pr/<n>` namespace is reserved as local-only.
        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["git", "remote", "list"],
                "origin\tgit@github.com:spinyfin/mono.git\n",
            ),
            // No further commands: the guard fires before jj git push.
        ]);

        let cli = Cli::parse_from(["cube", "pr", "ensure", "--branch", "pr/42"]);
        let err = run_with_dependencies(cli, None, &runner)
            .expect_err("ensure_pr should refuse a pr/* branch");
        runner.assert_exhausted();
        let msg = err.to_string();
        assert!(
            msg.contains("pr/42") && msg.contains("reserved"),
            "error should mention the bookmark and reserved: {msg}"
        );
    }

    #[test]
    fn ensure_pr_sets_pr_bookmark_on_create() {
        // After a new PR is created, `jj bookmark set pr/<n> -r <branch>` must
        // be called so the workspace has a local pointer from number to head.
        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["git", "remote", "list"],
                "origin\tgit@github.com:spinyfin/mono.git\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &[
                    "git", "push", "-b", "my-feature", "--remote", "origin", "--allow-new",
                ],
                "",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
                "abc123\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "api",
                    "repos/spinyfin/mono/branches/my-feature",
                    "--jq",
                    ".commit.sha",
                ],
                "abc123\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "pr", "list", "-R", "spinyfin/mono", "--head", "my-feature", "--state",
                    "open", "--json", "url",
                ],
                "[]",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "pr", "create", "-R", "spinyfin/mono", "--head", "my-feature", "--base",
                    "main", "--title", "My Feature",
                ],
                "https://github.com/spinyfin/mono/pull/55",
            ),
            // Must call `jj bookmark set pr/55 -r my-feature` after creation.
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["bookmark", "set", "pr/55", "-r", "my-feature"],
                "",
            ),
        ]);

        let cli = Cli::parse_from([
            "cube", "pr", "ensure", "--branch", "my-feature", "--title", "My Feature",
        ]);
        let result = run_with_dependencies(cli, None, &runner).expect("ensure_pr create+bookmark");
        runner.assert_exhausted();

        assert_eq!(result.payload["pr_bookmark"], "pr/55");
    }

    #[test]
    fn ensure_pr_sets_pr_bookmark_on_existing_pr() {
        // When a PR already exists (backfill path), `jj bookmark set pr/<n> -r
        // <branch>` must still be called so the local bookmark is up to date.
        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["git", "remote", "list"],
                "origin\tgit@github.com:spinyfin/mono.git\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &[
                    "git", "push", "-b", "my-feature", "--remote", "origin", "--allow-new",
                ],
                "",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
                "abc123\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "api",
                    "repos/spinyfin/mono/branches/my-feature",
                    "--jq",
                    ".commit.sha",
                ],
                "abc123\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "pr", "list", "-R", "spinyfin/mono", "--head", "my-feature", "--state",
                    "open", "--json", "url",
                ],
                r#"[{"url":"https://github.com/spinyfin/mono/pull/33"}]"#,
            ),
            // Bookmark set must happen even on the reuse/backfill path.
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["bookmark", "set", "pr/33", "-r", "my-feature"],
                "",
            ),
        ]);

        let cli = Cli::parse_from([
            "cube", "pr", "ensure", "--branch", "my-feature", "--title", "My Feature",
        ]);
        let result =
            run_with_dependencies(cli, None, &runner).expect("ensure_pr exists+bookmark");
        runner.assert_exhausted();

        assert_eq!(result.payload["action"], "exists");
        assert_eq!(result.payload["pr_bookmark"], "pr/33");
    }

    #[test]
    fn ensure_pr_errors_on_multiple_open_prs() {
        // If `gh pr list` returns more than one open PR for the branch, cube
        // must error rather than silently picking one. This is an unexpected
        // state that requires human intervention.
        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["git", "remote", "list"],
                "origin\tgit@github.com:spinyfin/mono.git\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &[
                    "git", "push", "-b", "my-feature", "--remote", "origin", "--allow-new",
                ],
                "",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "jj",
                &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
                "abc123\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "api",
                    "repos/spinyfin/mono/branches/my-feature",
                    "--jq",
                    ".commit.sha",
                ],
                "abc123\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(),
                "gh",
                &[
                    "pr", "list", "-R", "spinyfin/mono", "--head", "my-feature", "--state",
                    "open", "--json", "url",
                ],
                r#"[{"url":"https://github.com/spinyfin/mono/pull/10"},{"url":"https://github.com/spinyfin/mono/pull/11"}]"#,
            ),
        ]);

        let cli = Cli::parse_from([
            "cube", "pr", "ensure", "--branch", "my-feature", "--title", "My Feature",
        ]);
        let err = run_with_dependencies(cli, None, &runner)
            .expect_err("ensure_pr should fail on >1 open PRs");
        runner.assert_exhausted();
        let msg = err.to_string();
        assert!(
            msg.contains("2") && msg.contains("my-feature"),
            "error should mention count and branch: {msg}"
        );
    }

    // --- pr_push tests ---

    /// Build the standard remote-list response for a github-remote workspace.
    fn remote_list_github() -> &'static str {
        "origin\t/Users/bduff/dev/agents/repos/mono\ngithub\tgit@github.com:spinyfin/mono.git\n"
    }

    #[test]
    fn pr_push_happy_path_advance() {
        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            // remote list → github remote
            ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
            // check PR is open
            ExpectedCommand::ok(
                cwd.clone(), "gh",
                &["pr", "view", "42", "-R", "spinyfin/mono", "--json", "state"],
                r#"{"state":"OPEN"}"#,
            ),
            // @ is not empty
            ExpectedCommand::ok(cwd.clone(), "jj", &["log", "-r", "@", "--no-graph", "-T", "empty"], "false"),
            // ancestor check: pr/42 is an ancestor of @
            ExpectedCommand::ok(
                cwd.clone(), "jj",
                &["log", "-r", "pr/42 & ancestors(@)", "--no-graph", "-T", "commit_id"],
                "aabbcc\n",
            ),
            // advance head-branch bookmark
            ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "boss/exec_abc", "-r", "@"], ""),
            // advance pr/42 bookmark
            ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "pr/42", "-r", "@"], ""),
            // push (no --allow-new)
            ExpectedCommand::ok(
                cwd.clone(), "jj",
                &["git", "push", "-b", "boss/exec_abc", "--remote", "github"],
                "",
            ),
            // verify: local sha
            ExpectedCommand::ok(
                cwd.clone(), "jj",
                &["log", "-r", "boss/exec_abc", "--no-graph", "-T", "commit_id"],
                "deadbeef\n",
            ),
            // verify: github sha
            ExpectedCommand::ok(
                cwd.clone(), "gh",
                &["api", "repos/spinyfin/mono/branches/boss/exec_abc", "--jq", ".commit.sha"],
                "deadbeef\n",
            ),
        ]);

        let cli = Cli::parse_from([
            "cube", "pr", "push", "--pr", "42", "--branch", "boss/exec_abc",
        ]);
        let result = run_with_dependencies(cli, None, &runner).expect("pr_push happy path");
        runner.assert_exhausted();
        assert_eq!(result.payload["action"], "pushed");
        assert_eq!(result.payload["number"], 42);
        assert!(result.payload["url"].as_str().unwrap().contains("/pull/42"));
    }

    #[test]
    fn pr_push_noop_idempotency() {
        // @ is empty AND pr/42 sha matches GitHub head → no-op success.
        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
            ExpectedCommand::ok(
                cwd.clone(), "gh",
                &["pr", "view", "42", "-R", "spinyfin/mono", "--json", "state"],
                r#"{"state":"OPEN"}"#,
            ),
            // @ is empty
            ExpectedCommand::ok(cwd.clone(), "jj", &["log", "-r", "@", "--no-graph", "-T", "empty"], "true"),
            // fetch github sha for head-branch
            ExpectedCommand::ok(
                cwd.clone(), "gh",
                &["api", "repos/spinyfin/mono/branches/boss/exec_abc", "--jq", ".commit.sha"],
                "abc123\n",
            ),
            // fetch pr/42 sha — matches github
            ExpectedCommand::ok(
                cwd.clone(), "jj",
                &["log", "-r", "pr/42", "--no-graph", "-T", "commit_id"],
                "abc123\n",
            ),
        ]);

        let cli = Cli::parse_from([
            "cube", "pr", "push", "--pr", "42", "--branch", "boss/exec_abc",
        ]);
        let result = run_with_dependencies(cli, None, &runner).expect("pr_push noop");
        runner.assert_exhausted();
        assert_eq!(result.payload["action"], "noop");
        assert_eq!(result.payload["number"], 42);
    }

    #[test]
    fn pr_push_empty_at_nothing_to_land() {
        // @ is empty AND pr/42 sha does NOT match GitHub head → error.
        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
            ExpectedCommand::ok(
                cwd.clone(), "gh",
                &["pr", "view", "42", "-R", "spinyfin/mono", "--json", "state"],
                r#"{"state":"OPEN"}"#,
            ),
            ExpectedCommand::ok(cwd.clone(), "jj", &["log", "-r", "@", "--no-graph", "-T", "empty"], "true"),
            ExpectedCommand::ok(
                cwd.clone(), "gh",
                &["api", "repos/spinyfin/mono/branches/boss/exec_abc", "--jq", ".commit.sha"],
                "github_sha\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(), "jj",
                &["log", "-r", "pr/42", "--no-graph", "-T", "commit_id"],
                "local_sha\n",
            ),
        ]);

        let cli = Cli::parse_from([
            "cube", "pr", "push", "--pr", "42", "--branch", "boss/exec_abc",
        ]);
        let err = run_with_dependencies(cli, None, &runner).expect_err("should fail — nothing to land");
        runner.assert_exhausted();
        assert!(
            err.to_string().contains("empty") && err.to_string().contains("nothing to land"),
            "error should mention empty and nothing to land: {err}"
        );
    }

    #[test]
    fn pr_push_detached_refusal() {
        // @ is not a descendant of pr/42 → refuse.
        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
            ExpectedCommand::ok(
                cwd.clone(), "gh",
                &["pr", "view", "42", "-R", "spinyfin/mono", "--json", "state"],
                r#"{"state":"OPEN"}"#,
            ),
            ExpectedCommand::ok(cwd.clone(), "jj", &["log", "-r", "@", "--no-graph", "-T", "empty"], "false"),
            // ancestor check returns empty → not a descendant
            ExpectedCommand::ok(
                cwd.clone(), "jj",
                &["log", "-r", "pr/42 & ancestors(@)", "--no-graph", "-T", "commit_id"],
                "",
            ),
        ]);

        let cli = Cli::parse_from([
            "cube", "pr", "push", "--pr", "42", "--branch", "boss/exec_abc",
        ]);
        let err = run_with_dependencies(cli, None, &runner).expect_err("should refuse detached @");
        runner.assert_exhausted();
        assert!(
            err.to_string().contains("not a descendant") || err.to_string().contains("descendant"),
            "error should mention descendant: {err}"
        );
    }

    #[test]
    fn pr_push_stale_push_error() {
        // @ is non-empty, is a descendant, but jj git push fails (stale remote head).
        let cwd = std::env::current_dir().expect("cwd");
        let push_err = CubeError::CommandFailed {
            program: "jj".to_string(),
            args: vec!["git".to_string(), "push".to_string(), "-b".to_string(),
                       "boss/exec_abc".to_string(), "--remote".to_string(), "github".to_string()],
            status: Some(1),
            stderr: "Error: Remote bookmark boss/exec_abc@github is ahead of local bookmark"
                .to_string(),
        };
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
            ExpectedCommand::ok(
                cwd.clone(), "gh",
                &["pr", "view", "42", "-R", "spinyfin/mono", "--json", "state"],
                r#"{"state":"OPEN"}"#,
            ),
            ExpectedCommand::ok(cwd.clone(), "jj", &["log", "-r", "@", "--no-graph", "-T", "empty"], "false"),
            ExpectedCommand::ok(
                cwd.clone(), "jj",
                &["log", "-r", "pr/42 & ancestors(@)", "--no-graph", "-T", "commit_id"],
                "aabbcc\n",
            ),
            ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "boss/exec_abc", "-r", "@"], ""),
            ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "pr/42", "-r", "@"], ""),
            // push fails
            ExpectedCommand {
                cwd: cwd.clone(),
                program: "jj".to_string(),
                args: ["git", "push", "-b", "boss/exec_abc", "--remote", "github"]
                    .iter().map(|s| s.to_string()).collect(),
                result: Err(push_err),
                creates_dir: None,
            },
        ]);

        let cli = Cli::parse_from([
            "cube", "pr", "push", "--pr", "42", "--branch", "boss/exec_abc",
        ]);
        let err = run_with_dependencies(cli, None, &runner).expect_err("should surface push error");
        runner.assert_exhausted();
        assert!(
            err.to_string().contains("push") || err.to_string().contains("boss/exec_abc"),
            "error should mention push failure: {err}"
        );
    }

    #[test]
    fn pr_push_merged_pr_hard_error() {
        // PR is MERGED → hard error, no push attempted.
        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
            ExpectedCommand::ok(
                cwd.clone(), "gh",
                &["pr", "view", "42", "-R", "spinyfin/mono", "--json", "state"],
                r#"{"state":"MERGED"}"#,
            ),
        ]);

        let cli = Cli::parse_from([
            "cube", "pr", "push", "--pr", "42", "--branch", "boss/exec_abc",
        ]);
        let err = run_with_dependencies(cli, None, &runner).expect_err("should hard-error on merged PR");
        runner.assert_exhausted();
        assert!(
            err.to_string().contains("MERGED") || err.to_string().contains("merged"),
            "error should mention MERGED: {err}"
        );
    }

    #[test]
    fn pr_push_closed_pr_hard_error() {
        // PR is CLOSED → hard error.
        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
            ExpectedCommand::ok(
                cwd.clone(), "gh",
                &["pr", "view", "42", "-R", "spinyfin/mono", "--json", "state"],
                r#"{"state":"CLOSED"}"#,
            ),
        ]);

        let cli = Cli::parse_from([
            "cube", "pr", "push", "--pr", "42", "--branch", "boss/exec_abc",
        ]);
        let err = run_with_dependencies(cli, None, &runner).expect_err("should hard-error on closed PR");
        runner.assert_exhausted();
        assert!(
            err.to_string().contains("CLOSED") || err.to_string().contains("non-open"),
            "error should mention closed/non-open: {err}"
        );
    }

    #[test]
    fn pr_push_force_with_lease_happy_path() {
        // --force-with-lease: lease valid (fetched sha == github sha) → force push succeeds.
        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
            ExpectedCommand::ok(
                cwd.clone(), "gh",
                &["pr", "view", "42", "-R", "spinyfin/mono", "--json", "state"],
                r#"{"state":"OPEN"}"#,
            ),
            // @ is not empty
            ExpectedCommand::ok(cwd.clone(), "jj", &["log", "-r", "@", "--no-graph", "-T", "empty"], "false"),
            // lease check: jj's view of remote tracking bookmark
            ExpectedCommand::ok(
                cwd.clone(), "jj",
                &["log", "-r", "boss/exec_abc@github", "--no-graph", "-T", "commit_id"],
                "remote_sha\n",
            ),
            // lease check: GitHub's actual head — matches jj's view
            ExpectedCommand::ok(
                cwd.clone(), "gh",
                &["api", "repos/spinyfin/mono/branches/boss/exec_abc", "--jq", ".commit.sha"],
                "remote_sha\n",
            ),
            // advance bookmarks
            ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "boss/exec_abc", "-r", "@"], ""),
            ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "pr/42", "-r", "@"], ""),
            // force push via git
            ExpectedCommand::ok(
                cwd.clone(), "git",
                &["push", "--force-with-lease", "github", "boss/exec_abc"],
                "",
            ),
            // verify
            ExpectedCommand::ok(
                cwd.clone(), "jj",
                &["log", "-r", "boss/exec_abc", "--no-graph", "-T", "commit_id"],
                "new_sha\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(), "gh",
                &["api", "repos/spinyfin/mono/branches/boss/exec_abc", "--jq", ".commit.sha"],
                "new_sha\n",
            ),
        ]);

        let cli = Cli::parse_from([
            "cube", "pr", "push", "--pr", "42", "--branch", "boss/exec_abc", "--force-with-lease",
        ]);
        let result = run_with_dependencies(cli, None, &runner).expect("force-with-lease happy path");
        runner.assert_exhausted();
        assert_eq!(result.payload["action"], "pushed");
        assert_eq!(result.payload["number"], 42);
    }

    #[test]
    fn pr_push_force_with_lease_concurrent_advance_refusal() {
        // --force-with-lease: GitHub has advanced beyond last fetch → refuse.
        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
            ExpectedCommand::ok(
                cwd.clone(), "gh",
                &["pr", "view", "42", "-R", "spinyfin/mono", "--json", "state"],
                r#"{"state":"OPEN"}"#,
            ),
            ExpectedCommand::ok(cwd.clone(), "jj", &["log", "-r", "@", "--no-graph", "-T", "empty"], "false"),
            // lease check: jj's view of remote
            ExpectedCommand::ok(
                cwd.clone(), "jj",
                &["log", "-r", "boss/exec_abc@github", "--no-graph", "-T", "commit_id"],
                "old_sha\n",
            ),
            // lease check: GitHub advanced concurrently
            ExpectedCommand::ok(
                cwd.clone(), "gh",
                &["api", "repos/spinyfin/mono/branches/boss/exec_abc", "--jq", ".commit.sha"],
                "new_sha_from_concurrent_push\n",
            ),
        ]);

        let cli = Cli::parse_from([
            "cube", "pr", "push", "--pr", "42", "--branch", "boss/exec_abc", "--force-with-lease",
        ]);
        let err = run_with_dependencies(cli, None, &runner).expect_err("should refuse concurrent advance");
        runner.assert_exhausted();
        assert!(
            err.to_string().contains("force-with-lease refused")
                || err.to_string().contains("advanced"),
            "error should mention lease refusal: {err}"
        );
    }

    #[test]
    fn pr_push_infers_from_ancestry() {
        // No --pr / --branch: infer from jj ancestry.
        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
            // Inference query
            ExpectedCommand::ok(
                cwd.clone(), "jj",
                &[
                    "log", "-r", r#"latest(ancestors(@) & bookmarks(glob:"pr/*"))"#,
                    "--no-graph", "-T", r#"bookmarks.map(|b| b.name()).join("\n")"#,
                ],
                "boss/exec_abc\npr/42\n",
            ),
            // check PR is open
            ExpectedCommand::ok(
                cwd.clone(), "gh",
                &["pr", "view", "42", "-R", "spinyfin/mono", "--json", "state"],
                r#"{"state":"OPEN"}"#,
            ),
            // @ is not empty
            ExpectedCommand::ok(cwd.clone(), "jj", &["log", "-r", "@", "--no-graph", "-T", "empty"], "false"),
            // ancestor check
            ExpectedCommand::ok(
                cwd.clone(), "jj",
                &["log", "-r", "pr/42 & ancestors(@)", "--no-graph", "-T", "commit_id"],
                "aabbcc\n",
            ),
            // advance bookmarks
            ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "boss/exec_abc", "-r", "@"], ""),
            ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "pr/42", "-r", "@"], ""),
            // push
            ExpectedCommand::ok(
                cwd.clone(), "jj",
                &["git", "push", "-b", "boss/exec_abc", "--remote", "github"],
                "",
            ),
            // verify
            ExpectedCommand::ok(
                cwd.clone(), "jj",
                &["log", "-r", "boss/exec_abc", "--no-graph", "-T", "commit_id"],
                "deadbeef\n",
            ),
            ExpectedCommand::ok(
                cwd.clone(), "gh",
                &["api", "repos/spinyfin/mono/branches/boss/exec_abc", "--jq", ".commit.sha"],
                "deadbeef\n",
            ),
        ]);

        let cli = Cli::parse_from(["cube", "pr", "push"]);
        let result = run_with_dependencies(cli, None, &runner).expect("pr_push inferred from ancestry");
        runner.assert_exhausted();
        assert_eq!(result.payload["action"], "pushed");
        assert_eq!(result.payload["number"], 42);
    }

    #[test]
    fn pr_push_guard_rejects_pr_bookmark_head_branch() {
        // If the resolved head-branch is a pr/* name (explicit --branch pr/42), refuse.
        let cwd = std::env::current_dir().expect("cwd");
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
        ]);

        let cli = Cli::parse_from([
            "cube", "pr", "push", "--pr", "42", "--branch", "pr/42",
        ]);
        let err = run_with_dependencies(cli, None, &runner).expect_err("should refuse pr/* branch");
        runner.assert_exhausted();
        assert!(
            err.to_string().contains("reserved") || err.to_string().contains("pr/42"),
            "error should mention reserved namespace: {err}"
        );
    }

    // --- pr_number_from_url tests ---

    #[test]
    fn pr_number_from_url_parses_standard_url() {
        assert_eq!(
            super::pr_number_from_url("https://github.com/owner/repo/pull/123"),
            Some(123)
        );
    }

    #[test]
    fn pr_number_from_url_parses_url_with_trailing_slash() {
        assert_eq!(
            super::pr_number_from_url("https://github.com/owner/repo/pull/456/"),
            Some(456)
        );
    }

    #[test]
    fn pr_number_from_url_returns_none_for_non_numeric_suffix() {
        assert_eq!(
            super::pr_number_from_url("https://github.com/owner/repo/pull/"),
            None
        );
    }

    #[test]
    fn boss_infra_exclude_block_names_the_per_workspace_log() {
        let block = render_boss_infra_exclude_block("rdev-base-image-agent-001");
        assert!(block.contains("/logs/rdev-base-image-agent-001.log"));
        assert!(block.contains(".boss/"));
        assert!(block.starts_with(BOSS_INFRA_EXCLUDE_BEGIN));
        assert!(block.trim_end().ends_with(BOSS_INFRA_EXCLUDE_END));
    }

    #[test]
    fn upsert_managed_exclude_appends_to_empty_body() {
        let block = render_boss_infra_exclude_block("mono-agent-004");
        assert_eq!(upsert_managed_exclude("", &block), block);
    }

    #[test]
    fn upsert_managed_exclude_preserves_operator_excludes() {
        let block = render_boss_infra_exclude_block("mono-agent-004");
        let existing = "# operator-added\n*.tmp\n";
        let result = upsert_managed_exclude(existing, &block);
        assert!(result.starts_with("# operator-added\n*.tmp\n"));
        assert!(result.contains("/logs/mono-agent-004.log"));
    }

    #[test]
    fn upsert_managed_exclude_is_idempotent() {
        let block = render_boss_infra_exclude_block("mono-agent-004");
        let once = upsert_managed_exclude("*.tmp\n", &block);
        let twice = upsert_managed_exclude(&once, &block);
        assert_eq!(once, twice);
        // The managed marker appears exactly once after repeated rewrites.
        assert_eq!(twice.matches(BOSS_INFRA_EXCLUDE_BEGIN).count(), 1);
    }

    #[test]
    fn upsert_managed_exclude_rewrites_stale_block_in_place() {
        let stale = render_boss_infra_exclude_block("old-workspace-id");
        let existing = format!("*.tmp\n{stale}# trailing operator line\n");
        let fresh = render_boss_infra_exclude_block("new-workspace-id");
        let result = upsert_managed_exclude(&existing, &fresh);
        assert!(result.contains("/logs/new-workspace-id.log"));
        assert!(!result.contains("old-workspace-id"));
        // Operator content on both sides of the block survives.
        assert!(result.starts_with("*.tmp\n"));
        assert!(result.contains("# trailing operator line\n"));
        assert_eq!(result.matches(BOSS_INFRA_EXCLUDE_BEGIN).count(), 1);
    }

    #[test]
    fn ensure_boss_infra_excluded_writes_git_info_exclude() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let workspace = tempdir.path().join("mono-agent-004");
        std::fs::create_dir_all(workspace.join(".git")).expect("colocated .git dir");

        ensure_boss_infra_excluded(&workspace, "mono-agent-004");

        let exclude = std::fs::read_to_string(workspace.join(".git/info/exclude"))
            .expect("exclude written");
        assert!(exclude.contains("/logs/mono-agent-004.log"));
        assert!(exclude.contains(".boss/"));

        // Second call is a no-op: same bytes, single managed block.
        ensure_boss_infra_excluded(&workspace, "mono-agent-004");
        let again = std::fs::read_to_string(workspace.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude, again);
        assert_eq!(again.matches(BOSS_INFRA_EXCLUDE_BEGIN).count(), 1);
    }

    #[test]
    fn ensure_boss_infra_excluded_skips_when_not_colocated() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let workspace = tempdir.path().join("mono-agent-004");
        // Secondary jj workspace: `.jj` but no colocated `.git` directory.
        std::fs::create_dir_all(workspace.join(".jj")).expect("jj dir");

        ensure_boss_infra_excluded(&workspace, "mono-agent-004");

        assert!(!workspace.join(".git").exists());
    }

    // ── unhealthy GC tests ────────────────────────────────────────────────────

    fn setup_unhealthy_gc_scenario(
        tempdir: &TempDir,
        database_path: &std::path::Path,
    ) -> (crate::store::Store, std::path::PathBuf) {
        use crate::metadata::{RepoRecord, WorkspaceCandidate};
        use crate::store::Store;

        let workspace_root = tempdir.path().join("workspaces");
        let ws_path = workspace_root.join("mono-agent-001");
        std::fs::create_dir_all(ws_path.join(".jj")).expect("workspace dir");

        let mut store = Store::open_at(database_path).expect("store");
        store
            .upsert_repo(&RepoRecord {
                repo: "mono".to_string(),
                origin: "git@github.com:spinyfin/mono.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: workspace_root.clone(),
                workspace_prefix: "mono-agent-".to_string(),
                source: None,
                clone_command: None,
            })
            .expect("repo");
        store
            .sync_workspaces(
                "mono",
                &[WorkspaceCandidate {
                    workspace_id: "mono-agent-001".to_string(),
                    workspace_path: ws_path.clone(),
                }],
            )
            .expect("sync");

        (store, ws_path)
    }

    fn reset_runner_for(ws_path: &std::path::Path) -> FakeRunner {
        FakeRunner::new(vec![
            ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(
                ws_path.to_path_buf(),
                "jj",
                &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
                "",
            ),
            ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["new", "main"], ""),
        ])
    }

    #[test]
    fn gc_resets_aged_dirty_workspace_to_clean() {
        let (tempdir, database_path) = with_database_path();
        let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

        store
            .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Dirty)
            .expect("mark dirty");

        // Verify unhealthy_since was set.
        let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
        assert_eq!(ws.health_status, Some(crate::metadata::WorkspaceHealth::Dirty));
        assert!(ws.unhealthy_since_epoch_s.is_some(), "unhealthy_since should be set");

        // Simulate GC running 6 days later (threshold = 5 days).
        let fake_now = ws.unhealthy_since_epoch_s.unwrap() + 6 * 86_400;
        let max_age_secs = 5 * 86_400;

        let runner = reset_runner_for(&ws_path);
        gc_aged_unhealthy_workspaces(&runner, &store, Some(&database_path), fake_now, max_age_secs);
        runner.assert_exhausted();

        let ws_after = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
        assert_eq!(
            ws_after.health_status, None,
            "health_status should be cleared after GC reset"
        );
        assert_eq!(
            ws_after.unhealthy_since_epoch_s, None,
            "unhealthy_since_epoch_s should be cleared after GC reset"
        );
        assert_eq!(ws_after.state, crate::metadata::WorkspaceState::Free);
    }

    #[test]
    fn gc_resets_aged_conflicted_workspace_to_clean() {
        let (tempdir, database_path) = with_database_path();
        let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

        store
            .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Conflicted)
            .expect("mark conflicted");

        let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
        assert_eq!(ws.health_status, Some(crate::metadata::WorkspaceHealth::Conflicted));
        assert!(ws.unhealthy_since_epoch_s.is_some());

        let fake_now = ws.unhealthy_since_epoch_s.unwrap() + 6 * 86_400;
        let max_age_secs = 5 * 86_400;

        let runner = reset_runner_for(&ws_path);
        gc_aged_unhealthy_workspaces(&runner, &store, Some(&database_path), fake_now, max_age_secs);
        runner.assert_exhausted();

        let ws_after = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
        assert_eq!(ws_after.health_status, None);
        assert_eq!(ws_after.unhealthy_since_epoch_s, None);
    }

    #[test]
    fn gc_skips_recently_unhealthy_workspace() {
        let (tempdir, database_path) = with_database_path();
        let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

        store
            .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Dirty)
            .expect("mark dirty");

        let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
        let unhealthy_since = ws.unhealthy_since_epoch_s.unwrap();

        // GC runs only 1 day after unhealthy_since; threshold is 5 days.
        let fake_now = unhealthy_since + 86_400;
        let max_age_secs = 5 * 86_400;

        // No reset commands should be issued.
        let runner = FakeRunner::default();
        gc_aged_unhealthy_workspaces(&runner, &store, Some(&database_path), fake_now, max_age_secs);
        runner.assert_exhausted();

        let ws_after = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
        assert_eq!(
            ws_after.health_status,
            Some(crate::metadata::WorkspaceHealth::Dirty),
            "recent unhealthy workspace should be left untouched"
        );
        assert_eq!(ws_after.unhealthy_since_epoch_s, Some(unhealthy_since));
    }

    #[test]
    fn unhealthy_since_preserved_through_dirty_to_conflicted_transition() {
        let (tempdir, database_path) = with_database_path();
        let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

        store
            .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Dirty)
            .expect("mark dirty");

        let ws_after_dirty = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
        let original_since = ws_after_dirty.unhealthy_since_epoch_s.unwrap();

        // Transition to conflicted without becoming clean first.
        store
            .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Conflicted)
            .expect("mark conflicted");

        let ws_after_conflicted = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
        assert_eq!(
            ws_after_conflicted.health_status,
            Some(crate::metadata::WorkspaceHealth::Conflicted)
        );
        assert_eq!(
            ws_after_conflicted.unhealthy_since_epoch_s,
            Some(original_since),
            "unhealthy_since should not be reset when transitioning between unhealthy states"
        );
    }
}
