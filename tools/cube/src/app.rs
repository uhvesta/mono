use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use console::{Style, style};
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::audit;
use crate::cli::{
    ChangeCommand, Cli, Command, DoctorArgs, GraphArgs, PrCommand, RepoCommand, StackCommand,
    WorkspaceCommand,
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
        Command::Pr { command } => run_pr(command),
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
        RepoCommand::Ensure { origin } => {
            let origin = normalize_origin(&origin)?;
            let defaults = if let Some(defaults) = repo_ensure_defaults {
                defaults.clone()
            } else {
                default_repo_ensure_defaults()?
            };
            let record = ensure_repo(&store, runner, &origin, &defaults, cube_config)?;
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

fn ensure_repo(
    store: &Store,
    runner: &dyn CommandRunner,
    origin: &str,
    defaults: &RepoEnsureDefaults,
    cube_config: Option<config::CubeConfig>,
) -> Result<RepoRecord> {
    let cfg = match cube_config {
        Some(c) => c,
        None => config::load_config()?,
    };

    if let Some(record) = store.get_repo_by_origin(origin)? {
        fs::create_dir_all(&record.workspace_root).map_err(|e| CubeError::WorkspaceDirCreate {
            path: record.workspace_root.clone(),
            source: e,
        })?;
        materialize_repo_source_if_missing(runner, &record, &cfg)?;
        return Ok(record);
    }

    let record = infer_repo_record_from_origin(origin, defaults)?;
    if let Some(existing) = store.get_repo(&record.repo)? {
        // Treat two URLs as equivalent when they differ only in auth-identity
        // prefix (e.g. `org-X@github.com:` vs `git@github.com:`). Corporate
        // git configs rewrite remote URLs with an org-specific user prefix, so
        // the stored origin and the incoming origin may not match exactly even
        // when they point at the same repo.
        if !origin_urls_equivalent(&existing.origin, origin) {
            return Err(CubeError::InvalidArgument(format!(
                "repo `{}` is already configured for origin `{}`; cannot ensure `{origin}`",
                existing.repo, existing.origin
            )));
        }
        fs::create_dir_all(&existing.workspace_root).map_err(|e| CubeError::WorkspaceDirCreate {
            path: existing.workspace_root.clone(),
            source: e,
        })?;
        materialize_repo_source_if_missing(runner, &existing, &cfg)?;
        return Ok(existing);
    }

    fs::create_dir_all(&record.workspace_root).map_err(|e| CubeError::WorkspaceDirCreate {
        path: record.workspace_root.clone(),
        source: e,
    })?;
    materialize_repo_source_if_missing(runner, &record, &cfg)?;
    store.upsert_repo(&record)
}

/// Parsed representation of a git remote URL, normalised for equivalence checks.
#[derive(Debug, PartialEq)]
struct ParsedOrigin {
    /// Lower-cased host (e.g. `github.com`).
    host: String,
    /// Repo path without leading slash and without trailing `.git`
    /// (e.g. `linkedin-sandbox/bduff`). Case-sensitive.
    path: String,
}

/// Parse an SSH-style (`[user@]host:path`), `ssh://` URL, or HTTPS-style
/// (`https://[user@]host/path`) URL into a `ParsedOrigin`. Returns `None` if
/// the URL is not in a recognised format.
fn parse_origin(url: &str) -> Option<ParsedOrigin> {
    let url = url.trim();

    // HTTPS: https://[user@]host/path
    if let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) {
        // Drop optional `user@`
        let rest = if let Some(at) = rest.find('@') {
            &rest[at + 1..]
        } else {
            rest
        };
        let (host, path) = rest.split_once('/')?;
        let path = path.trim_end_matches('/').trim_end_matches(".git");
        return Some(ParsedOrigin {
            host: host.to_ascii_lowercase(),
            path: path.to_string(),
        });
    }

    // RFC-3986 SSH URL: ssh://[user@]host[:port]/path
    if let Some(rest) = url.strip_prefix("ssh://") {
        // Drop optional `user@`
        let rest = if let Some(at) = rest.find('@') {
            &rest[at + 1..]
        } else {
            rest
        };
        // Drop optional `:port` from the host portion before the path `/`
        let (host_maybe_port, path) = rest.split_once('/')?;
        let host = if let Some((h, _port)) = host_maybe_port.split_once(':') {
            h
        } else {
            host_maybe_port
        };
        let path = path.trim_end_matches('/').trim_end_matches(".git");
        return Some(ParsedOrigin {
            host: host.to_ascii_lowercase(),
            path: path.to_string(),
        });
    }

    // SSH SCP-like: [user@]host:path
    // Must contain `:` but must NOT look like a Windows absolute path (`C:\`).
    if let Some(colon_pos) = url.find(':') {
        let before_colon = &url[..colon_pos];
        let after_colon = &url[colon_pos + 1..];
        // Reject Windows paths (single letter before colon) and paths starting with `//` (git+ssh://)
        if before_colon.len() > 1 && !after_colon.starts_with('/') {
            // Strip optional `user@` from the host part
            let host = if let Some(at) = before_colon.rfind('@') {
                &before_colon[at + 1..]
            } else {
                before_colon
            };
            let path = after_colon
                .trim_end_matches('/')
                .trim_end_matches(".git");
            return Some(ParsedOrigin {
                host: host.to_ascii_lowercase(),
                path: path.to_string(),
            });
        }
    }

    None
}

/// Returns `true` when two origin URL strings refer to the same repository,
/// ignoring auth-identity prefixes (e.g. `org-X@` vs `git@`) and trailing
/// `.git` suffixes. Host comparison is case-insensitive; path is case-sensitive.
fn origin_urls_equivalent(a: &str, b: &str) -> bool {
    match (parse_origin(a), parse_origin(b)) {
        (Some(pa), Some(pb)) => pa == pb,
        // If either URL is unparseable fall back to exact-string equality so
        // we never accidentally allow a mismatch.
        _ => a == b,
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

fn infer_repo_record_from_origin(
    origin: &str,
    defaults: &RepoEnsureDefaults,
) -> Result<RepoRecord> {
    let repo = repo_id_from_origin(origin)?;
    Ok(RepoRecord {
        repo: repo.clone(),
        origin: origin.to_string(),
        main_branch: "main".to_string(),
        workspace_root: defaults.workspace_root.clone(),
        workspace_prefix: format!("{repo}-agent-"),
        source: Some(defaults.repo_root.join(&repo)),
    })
}

fn materialize_repo_source_if_missing(
    runner: &dyn CommandRunner,
    record: &RepoRecord,
    config: &config::CubeConfig,
) -> Result<()> {
    let Some(source) = &record.source else {
        return Ok(());
    };

    if source.exists() {
        if source.is_dir() {
            return Ok(());
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

    let mp = &config.multiproduct;
    if mp.enabled && is_multiproduct_repo(&record.origin, &mp.org) {
        let repo_name = repo_name_for_mint_clone(&record.origin)?;
        if which::which(&mp.clone_command).is_err() {
            let config_path = config::config_file_path()
                .unwrap_or_else(|_| PathBuf::from("~/.config/cube/cube.toml"));
            return Err(CubeError::InvalidArgument(format!(
                "`{}` is not on PATH, but multiproduct cloning is enabled in `{}`. \
                 Install `{}` or set `[multiproduct] enabled = false` in that file.",
                mp.clone_command,
                config_path.display(),
                mp.clone_command,
            )));
        }
        eprintln!(
            "cube: using `{} clone` for multiproduct repo `{}`",
            mp.clone_command, repo_name
        );
        runner.run(&CommandInvocation {
            cwd: parent.to_path_buf(),
            program: mp.clone_command.clone(),
            args: vec!["clone".to_string(), repo_name],
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
    } else {
        eprintln!("cube: using `jj git clone` for repo `{}`", record.repo);
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
    }
    Ok(())
}

/// Returns true when the given origin URL identifies a multiproduct repo.
/// Detection uses two strategies:
/// - URL-based (option b): the remote path contains `/{org}/` or `:{org}/`
/// - Bare-name (option a): the origin has no URL separators (no `/`, `:`, or `@`),
///   i.e. the user passed a short repo name directly
fn is_multiproduct_repo(origin: &str, org: &str) -> bool {
    origin.contains(&format!("/{org}/"))
        || origin.contains(&format!(":{org}/"))
        || (!origin.contains('/') && !origin.contains(':') && !origin.contains('@'))
}

/// Extract the short repo name from an origin to pass to `mint clone`.
/// For a full URL like `org-127256988@github.com:linkedin-multiproduct/frontend-api.git`,
/// returns `"frontend-api"`. For a bare name like `"frontend-api"`, returns it as-is.
fn repo_name_for_mint_clone(origin: &str) -> Result<String> {
    let trimmed = origin.trim().trim_end_matches('/');
    let tail = trimmed
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(trimmed);
    let name = tail.strip_suffix(".git").unwrap_or(tail);
    if name.is_empty() {
        return Err(CubeError::InvalidArgument(format!(
            "could not extract repo name from origin `{origin}` for mint clone"
        )));
    }
    Ok(name.to_string())
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
        WorkspaceCommand::Lease { repo, task, prefer } => {
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
                if let Some(pref) = prefer.as_deref() {
                    if free_workspaces.iter().any(|w| w.workspace_id == pref) {
                        v.push(pref.to_string());
                    }
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
            let mut dirty_count = 0usize;
            // workspaces whose directory exists but has neither .jj/ nor .git/
            let mut broken_empty_count = 0usize;
            let mut broken_empty_paths: Vec<String> = Vec::new();

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
                        dirty_count += 1;
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
                        broken_empty_count += 1;
                        broken_empty_paths.push(format!(
                            "{ws_id} ({})",
                            ws.workspace_path.display()
                        ));
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

            // Decide which workspace to use: prefer clean, fall back to
            // first repairable conflicted workspace, otherwise auto-create
            // (pool empty) or error (pool all-dirty).
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
            } else if !ordered_ids.is_empty()
                && dirty_count + broken_empty_count == ordered_ids.len()
            {
                // Every free workspace is either dirty or broken-empty. Do NOT
                // auto-create — the operator needs to intervene. Return a
                // structured error so Boss can surface this as `blocked`.
                let mut parts: Vec<String> = Vec::new();
                if dirty_count > 0 {
                    parts.push(format!(
                        "{dirty_count} workspace(s) have dirty working copies \
                         (run `cube workspace force-release --reason crash` to reclaim)"
                    ));
                }
                if broken_empty_count > 0 {
                    parts.push(format!(
                        "{broken_empty_count} workspace(s) have neither .git/ nor .jj/ \
                         (broken-empty): {}; re-clone manually or force-release and retry",
                        broken_empty_paths.join(", ")
                    ));
                }
                return Err(CubeError::NoAvailableWorkspace(format!(
                    "{repo}: {}; run `cube workspace list --repo {repo}` to inspect",
                    parts.join("; ")
                )));
            } else {
                // Pool has no free workspaces (empty or all leased): auto-create.
                let new_candidate = auto_create_workspace(runner, &repo_record, &candidates)?;
                candidates.push(new_candidate);
                store.sync_workspaces(&repo, &candidates)?;
                let ws = store
                    .claim_workspace(
                        &repo,
                        &holder,
                        &task,
                        &lease_id,
                        leased_at_epoch_s,
                        lease_expires_at,
                        prefer.as_deref(),
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
            if !repair_bookmarks.is_empty() {
                if let Err(error) = repair_conflicted_bookmarks(
                    runner,
                    database_path,
                    &workspace.workspace_path,
                    &repair_bookmarks,
                ) {
                    let _ = store.release_workspace(&lease_id, Some("lease_setup_failed"));
                    return Err(error);
                }
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
            );

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
            RunResult::new(
                message,
                json!({
                    "workspace": workspace,
                    "setup": setup_report,
                    "health_check": health_checks,
                }),
            )
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

fn run_pr(command: PrCommand) -> Result<RunResult> {
    Err(CubeError::NotImplemented(format!(
        "pr command `{}` is not implemented yet",
        pr_command_name(&command)
    )))
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
        if let Some(suffix) = id.strip_prefix(prefix) {
            if let Ok(n) = suffix.parse::<u32>() {
                found_any = true;
                if n > max_n {
                    max_n = n;
                }
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
            workspace_path.display().to_string(),
        ],
    })?;

    Ok(crate::metadata::WorkspaceCandidate {
        workspace_id,
        workspace_path,
    })
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

/// List and optionally forget consumed `boss/exec_*` bookmarks in a workspace.
///
/// A bookmark is "consumed" when its tip is reachable from `main`
/// (`bookmarks(glob:"boss/exec_*") & ::main`). If `do_fetch` is true, runs
/// `jj git fetch` first so `::main` reflects the latest merged PRs. If
/// `dry_run` is true, lists what would be forgotten without acting.
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

    let bookmarks: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        output
            .split_whitespace()
            .filter(|s| s.starts_with("boss/exec_") && !s.contains('@'))
            .filter(|s| seen.insert(s.to_string()))
            .map(str::to_string)
            .collect()
    };

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
}

fn reset_workspace(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
    main_branch: &str,
) -> Result<()> {
    reset_workspace_guarded(runner, database_path, workspace_path, main_branch, None)
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

    audit_jj_op(database_path, workspace_path, "new", &[main_branch], prior_expired);
    run_jj(
        runner,
        database_path,
        &RealCommandRunner::invocation(workspace_path, "jj", &["new", main_branch]),
    )?;
    Ok(())
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
        CubeError, POOL_GC_LAST_AT_KEY, RepoEnsureDefaults, Result, current_epoch_s,
        origin_urls_equivalent, parse_origin, run_with_context, run_with_dependencies,
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

        let runner = FakeRunner::new(vec![ExpectedCommand::ok(
            defaults.repo_root.clone(),
            "jj",
            &[
                "git",
                "clone",
                "git@github.com:spinyfin/mono.git",
                &source_path.display().to_string(),
            ],
            "",
        )]);

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
        let runner = FakeRunner::new(vec![ExpectedCommand::ok(
            defaults.repo_root.clone(),
            "jj",
            &[
                "git",
                "clone",
                "git@github.com:spinyfin/mono.git",
                &source_path.display().to_string(),
            ],
            "",
        )]);

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

    fn multiproduct_config(enabled: bool) -> crate::config::CubeConfig {
        multiproduct_config_with_cmd(enabled, "mint")
    }

    fn multiproduct_config_with_cmd(enabled: bool, cmd: &str) -> crate::config::CubeConfig {
        crate::config::CubeConfig {
            multiproduct: crate::config::MultiproductConfig {
                enabled,
                clone_command: cmd.to_string(),
                org: "linkedin-multiproduct".to_string(),
            },
        }
    }

    #[test]
    fn repo_ensure_uses_mint_clone_for_multiproduct_url() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        let source_path = defaults.repo_root.join("frontend-api");
        let origin = "org-127256988@github.com:linkedin-multiproduct/frontend-api.git";

        // Use "true" as the clone command — it exists on PATH (/usr/bin/true)
        // so the which-check succeeds without mint being installed.
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
        ]);

        let ensure = Cli::parse_from(["cube", "repo", "ensure", "--origin", origin]);
        let result = run_with_context(
            ensure,
            Some(&database_path),
            &runner,
            Some(&defaults),
            Some(multiproduct_config_with_cmd(true, "true")),
        )
        .expect("ensure");

        assert_eq!(result.message, "Ensured repo `frontend-api`.");
        assert_eq!(result.payload["repo_id"], "frontend-api");
        runner.assert_exhausted();
    }

    #[test]
    fn repo_ensure_uses_mint_clone_for_bare_name() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        let source_path = defaults.repo_root.join("frontend-api");

        // Use "true" as the clone command so the which-check passes without mint installed.
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
        ]);

        let ensure =
            Cli::parse_from(["cube", "repo", "ensure", "--origin", "frontend-api"]);
        let result = run_with_context(
            ensure,
            Some(&database_path),
            &runner,
            Some(&defaults),
            Some(multiproduct_config_with_cmd(true, "true")),
        )
        .expect("ensure");

        assert_eq!(result.message, "Ensured repo `frontend-api`.");
        runner.assert_exhausted();
    }

    #[test]
    fn repo_ensure_uses_git_clone_for_non_multiproduct_url_even_when_flag_enabled() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        let source_path = defaults.repo_root.join("myrepo");
        let origin = "git@github.com:linkedin-sandbox/myrepo.git";

        let runner = FakeRunner::new(vec![ExpectedCommand::ok(
            defaults.repo_root.clone(),
            "jj",
            &[
                "git",
                "clone",
                origin,
                &source_path.display().to_string(),
            ],
            "",
        )]);

        let ensure = Cli::parse_from(["cube", "repo", "ensure", "--origin", origin]);
        let result = run_with_context(
            ensure,
            Some(&database_path),
            &runner,
            Some(&defaults),
            Some(multiproduct_config(true)),
        )
        .expect("ensure");

        assert_eq!(result.message, "Ensured repo `myrepo`.");
        runner.assert_exhausted();
    }

    #[test]
    fn repo_ensure_uses_git_clone_when_multiproduct_disabled() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        let source_path = defaults.repo_root.join("frontend-api");
        let origin = "org-127256988@github.com:linkedin-multiproduct/frontend-api.git";

        let runner = FakeRunner::new(vec![ExpectedCommand::ok(
            defaults.repo_root.clone(),
            "jj",
            &[
                "git",
                "clone",
                origin,
                &source_path.display().to_string(),
            ],
            "",
        )]);

        let ensure = Cli::parse_from(["cube", "repo", "ensure", "--origin", origin]);
        let result = run_with_context(
            ensure,
            Some(&database_path),
            &runner,
            Some(&defaults),
            Some(multiproduct_config(false)),
        )
        .expect("ensure");

        assert_eq!(result.message, "Ensured repo `frontend-api`.");
        runner.assert_exhausted();
    }

    #[test]
    fn is_multiproduct_repo_detection() {
        // URL-based detection
        assert!(super::is_multiproduct_repo(
            "org-127@github.com:linkedin-multiproduct/frontend-api.git",
            "linkedin-multiproduct"
        ));
        assert!(super::is_multiproduct_repo(
            "https://github.com/linkedin-multiproduct/frontend-api",
            "linkedin-multiproduct"
        ));
        // Bare name detection
        assert!(super::is_multiproduct_repo("frontend-api", "linkedin-multiproduct"));
        // Non-multiproduct URLs
        assert!(!super::is_multiproduct_repo(
            "git@github.com:linkedin-sandbox/myrepo.git",
            "linkedin-multiproduct"
        ));
        assert!(!super::is_multiproduct_repo(
            "git@github.com:spinyfin/mono.git",
            "linkedin-multiproduct"
        ));
    }

    #[test]
    fn repo_name_for_mint_clone_extracts_names() {
        assert_eq!(
            super::repo_name_for_mint_clone(
                "org-127@github.com:linkedin-multiproduct/frontend-api.git"
            )
            .unwrap(),
            "frontend-api"
        );
        assert_eq!(
            super::repo_name_for_mint_clone("frontend-api").unwrap(),
            "frontend-api"
        );
        assert_eq!(
            super::repo_name_for_mint_clone(
                "https://github.com/linkedin-multiproduct/frontend-api"
            )
            .unwrap(),
            "frontend-api"
        );
    }

    #[test]
    fn repo_ensure_errors_clearly_when_clone_command_not_on_path() {
        let (tempdir, database_path) = with_database_path();
        let defaults = repo_ensure_defaults(&tempdir);
        let origin = "org-127256988@github.com:linkedin-multiproduct/frontend-api.git";

        // Use a binary name that definitely does not exist on PATH.
        let ensure = Cli::parse_from(["cube", "repo", "ensure", "--origin", origin]);
        let err = run_with_context(
            ensure,
            Some(&database_path),
            &FakeRunner::default(),
            Some(&defaults),
            Some(multiproduct_config_with_cmd(
                true,
                "this-binary-does-not-exist-cube-test",
            )),
        )
        .expect_err("should fail when clone command is missing");

        let msg = err.to_string();
        assert!(
            msg.contains("this-binary-does-not-exist-cube-test"),
            "error should name the missing binary: {msg}"
        );
        assert!(
            msg.contains("not on PATH"),
            "error should mention PATH: {msg}"
        );
        assert!(
            msg.contains("multiproduct"),
            "error should reference multiproduct config: {msg}"
        );
    }

    // --- parse_origin / origin_urls_equivalent unit tests ---

    #[test]
    fn parse_origin_plain_ssh() {
        let p = parse_origin("git@github.com:foo/bar.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.path, "foo/bar");
    }

    #[test]
    fn parse_origin_auth_prefixed_ssh() {
        let p = parse_origin("org-132020694@github.com:linkedin-sandbox/bduff.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.path, "linkedin-sandbox/bduff");
    }

    #[test]
    fn parse_origin_ssh_no_dot_git() {
        let p = parse_origin("git@github.com:foo/bar").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.path, "foo/bar");
    }

    #[test]
    fn parse_origin_https() {
        let p = parse_origin("https://github.com/foo/bar.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.path, "foo/bar");
    }

    #[test]
    fn parse_origin_https_with_user() {
        let p = parse_origin("https://myuser@github.com/foo/bar.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.path, "foo/bar");
    }

    #[test]
    fn origin_urls_equivalent_plain_vs_auth_prefixed() {
        assert!(origin_urls_equivalent(
            "git@github.com:linkedin-sandbox/bduff.git",
            "org-132020694@github.com:linkedin-sandbox/bduff.git"
        ));
    }

    #[test]
    fn origin_urls_equivalent_auth_prefixed_vs_plain() {
        assert!(origin_urls_equivalent(
            "org-132020694@github.com:linkedin-sandbox/bduff.git",
            "git@github.com:linkedin-sandbox/bduff.git"
        ));
    }

    #[test]
    fn origin_urls_equivalent_dot_git_vs_no_dot_git() {
        assert!(origin_urls_equivalent(
            "git@github.com:foo/bar.git",
            "git@github.com:foo/bar"
        ));
    }

    #[test]
    fn origin_urls_not_equivalent_different_path() {
        assert!(!origin_urls_equivalent(
            "git@github.com:linkedin-sandbox/bduff.git",
            "git@github.com:linkedin-eng/bduff.git"
        ));
    }

    #[test]
    fn origin_urls_not_equivalent_different_host() {
        assert!(!origin_urls_equivalent(
            "git@github.com:foo/bar.git",
            "git@gitlab.com:foo/bar.git"
        ));
    }

    // --- ssh:// URL form tests ---

    #[test]
    fn parse_origin_ssh_url_form() {
        let p = parse_origin("ssh://git@github.com/foo/bar.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.path, "foo/bar");
    }

    #[test]
    fn parse_origin_ssh_url_auth_prefixed() {
        let p = parse_origin("ssh://org-132020694@github.com/linkedin-eng/ci-infra.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.path, "linkedin-eng/ci-infra");
    }

    #[test]
    fn parse_origin_ssh_url_no_user() {
        let p = parse_origin("ssh://github.com/foo/bar.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.path, "foo/bar");
    }

    #[test]
    fn parse_origin_ssh_url_with_port() {
        let p = parse_origin("ssh://git@github.com:22/foo/bar.git").unwrap();
        assert_eq!(p.host, "github.com");
        assert_eq!(p.path, "foo/bar");
    }

    #[test]
    fn origin_urls_equivalent_ssh_url_vs_scp() {
        // ssh://git@github.com/foo/bar.git == git@github.com:foo/bar.git
        assert!(origin_urls_equivalent(
            "ssh://git@github.com/foo/bar.git",
            "git@github.com:foo/bar.git"
        ));
    }

    #[test]
    fn origin_urls_equivalent_scp_vs_ssh_url() {
        assert!(origin_urls_equivalent(
            "git@github.com:foo/bar.git",
            "ssh://git@github.com/foo/bar.git"
        ));
    }

    #[test]
    fn origin_urls_equivalent_ssh_url_auth_vs_scp_plain() {
        // ssh://org-X@github.com/foo/bar.git == git@github.com:foo/bar.git
        assert!(origin_urls_equivalent(
            "ssh://org-132020694@github.com/linkedin-eng/ci-infra.git",
            "git@github.com:linkedin-eng/ci-infra.git"
        ));
    }

    #[test]
    fn origin_urls_equivalent_scp_auth_vs_ssh_url_plain() {
        assert!(origin_urls_equivalent(
            "org-132020694@github.com:linkedin-eng/ci-infra.git",
            "ssh://git@github.com/linkedin-eng/ci-infra.git"
        ));
    }

    #[test]
    fn origin_urls_equivalent_all_four_cross_products() {
        let variants = [
            "ssh://git@github.com/foo/bar.git",
            "ssh://org-132020694@github.com/foo/bar.git",
            "git@github.com:foo/bar.git",
            "org-132020694@github.com:foo/bar.git",
        ];
        for a in &variants {
            for b in &variants {
                assert!(
                    origin_urls_equivalent(a, b),
                    "{a} and {b} should be equivalent"
                );
            }
        }
    }

    #[test]
    fn origin_urls_not_equivalent_ssh_url_different_path() {
        assert!(!origin_urls_equivalent(
            "ssh://git@github.com/foo/bar.git",
            "git@github.com:foo/baz.git"
        ));
    }

    #[test]
    fn origin_urls_not_equivalent_ssh_url_different_host() {
        assert!(!origin_urls_equivalent(
            "ssh://git@github.com/foo/bar.git",
            "git@gitlab.com:foo/bar.git"
        ));
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
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "--colocate",
                    "git@github.com:spinyfin/mono.git",
                    &new_path.display().to_string(),
                ],
                "",
            )
            .creating_dir(new_path.clone()),
            ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
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
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "--colocate",
                    "git@github.com:spinyfin/mono.git",
                    &new_path.display().to_string(),
                ],
                "",
            )
            .creating_dir(new_path.clone()),
            ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
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
    fn workspace_lease_all_dirty_returns_structured_error() {
        // Pool: dirty(003), dirty(007) → no usable workspace → structured error.
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
            ExpectedCommand::ok(path_003.clone(), "jj", &["status", "--no-pager"], jj_status_dirty()),
            ExpectedCommand::ok(path_007.clone(), "jj", &["status", "--no-pager"], jj_status_dirty()),
        ]);

        let err = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "all dirty"]),
            Some(&database_path),
            &runner,
        )
        .expect_err("should fail when all workspaces are dirty");
        runner.assert_exhausted();

        match err {
            CubeError::NoAvailableWorkspace(msg) => {
                assert!(
                    msg.contains("dirty working copies"),
                    "error should mention dirty: {msg}"
                );
                assert!(
                    msg.contains("mono"),
                    "error should mention repo: {msg}"
                );
            }
            other => panic!("expected NoAvailableWorkspace, got: {other:?}"),
        }

        // Both workspaces should be marked dirty in the store.
        use crate::store::Store;
        let store = Store::open_at(&database_path).unwrap();
        for path in [&path_003, &path_007] {
            let ws = store.get_workspace_by_path(path).unwrap().unwrap();
            assert_eq!(
                ws.health_status,
                Some(crate::metadata::WorkspaceHealth::Dirty),
                "expected dirty health for {}",
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
            ExpectedCommand::ok(ws_path.clone(), "jj", &["new", "main"], ""),
            gc_noop_command(&ws_path),
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
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            gc_noop_command(&workspace_path),
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
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            gc_noop_command(&workspace_path),
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
        // Each reset emits a fetch + new pair, and we have a lease and
        // a release: so four `workspace.jj_op` entries on the timeline.
        let jj_ops: Vec<&serde_json::Value> = events
            .iter()
            .filter(|e| e["event"] == "workspace.jj_op")
            .collect();
        assert_eq!(jj_ops.len(), 4, "expected 4 workspace.jj_op events (fetch+new each for lease+release)");
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
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            gc_noop_command(&workspace_path),
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
        let lease_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "--colocate",
                    "git@github.com:spinyfin/mono.git",
                    &new_path.display().to_string(),
                ],
                "",
            )
            .creating_dir(new_path.clone()),
            ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
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

    /// Standard release runner: fetch, reset, then gc-noop.
    fn release_runner_for(workspace_path: &std::path::Path) -> FakeRunner {
        FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["new", "main"], ""),
            gc_noop_command(workspace_path),
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
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            gc_noop_command(&workspace_path),
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
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
            gc_noop_command(&workspace_path),
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
    fn workspace_lease_broken_empty_gives_clear_error_without_calling_jj() {
        // When a workspace directory has neither .jj/ nor .git/, cube detects
        // the broken-empty state via a directory check BEFORE calling jj,
        // and surfaces a clear NoAvailableWorkspace error naming the
        // workspace path and what's missing. jj is never invoked.
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        let workspace_path = workspace_root.join("mono-agent-004");
        // Intentionally no .jj/ or .git/ — this is the broken-empty state.
        std::fs::create_dir_all(&workspace_path).expect("workspace dir");

        run_with_dependencies(
            add_repo_cli(&workspace_root),
            Some(&database_path),
            &FakeRunner::default(),
        )
        .expect("repo");

        // FakeRunner has NO expected commands: cube must not call jj at all.
        let runner = FakeRunner::default();

        let error = run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "no git dir"]),
            Some(&database_path),
            &runner,
        )
        .expect_err("lease should fail with a clear error when workspace is broken-empty");
        runner.assert_exhausted();

        // Error must be NoAvailableWorkspace (not a raw CommandFailed from jj).
        assert!(
            matches!(error, CubeError::NoAvailableWorkspace(_)),
            "expected NoAvailableWorkspace, got: {error:?}"
        );
        // Error message must name the workspace path and the missing directories.
        let msg = error.to_string();
        assert!(
            msg.contains(workspace_path.to_str().unwrap()),
            "error should name the workspace path; got: {msg}"
        );
        assert!(
            msg.contains(".git") || msg.contains(".jj"),
            "error should mention missing .git/ or .jj/; got: {msg}"
        );

        // Audit log must contain the broken_empty event.
        let events = audit_events(&tempdir);
        assert!(
            events.iter().any(|e| e["event"] == "workspace.broken_empty"),
            "expected workspace.broken_empty audit event; got: {events:?}"
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
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                workspace_root.clone(),
                "jj",
                &[
                    "git",
                    "clone",
                    "--colocate",
                    "git@github.com:spinyfin/mono.git",
                    &new_path.display().to_string(),
                ],
                "",
            )
            .creating_dir(new_path.clone()),
            ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
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
}
