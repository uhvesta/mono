use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::cli::{
    ChangeCommand, Cli, Command, DoctorArgs, GraphArgs, PrCommand, RepoCommand, StackCommand,
    WorkspaceCommand,
};
use crate::command_runner::{CommandInvocation, CommandRunner, RealCommandRunner};
use crate::lock::RepoLock;
use crate::metadata::{ChangeRecord, RepoRecord, WorkspaceRecord, WorkspaceState};
use crate::paths;
use crate::store::{Store, WorkspaceListFilter};

type Result<T> = std::result::Result<T, CubeError>;

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
    #[error("failed to access Cube metadata: {0}")]
    Storage(#[source] rusqlite::Error),
    #[error("failed to prepare Cube data directory: {0}")]
    Io(#[from] std::io::Error),
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
}

impl CubeError {
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::InvalidArgument(_) | Self::NotImplemented(_) => ExitCode::from(2),
            Self::RepoNotFound(_) => ExitCode::from(3),
            Self::NoAvailableWorkspace(_) => ExitCode::from(4),
            Self::WorkspaceNotFound(_) | Self::LeaseNotFound(_) | Self::ChangeNotFound(_) => {
                ExitCode::from(5)
            }
            Self::Storage(_) | Self::Io(_) | Self::CommandFailed { .. } | Self::Json(_) => {
                ExitCode::FAILURE
            }
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
    run_with_context(cli, database_path, runner, None)
}

fn run_with_context(
    cli: Cli,
    database_path: Option<&Path>,
    runner: &dyn CommandRunner,
    repo_ensure_defaults: Option<&RepoEnsureDefaults>,
) -> Result<RunResult> {
    match cli.command {
        Command::Repo { command } => run_repo(command, database_path, runner, repo_ensure_defaults),
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
            let record = ensure_repo(&store, runner, &origin, &defaults)?;
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
            let message = if repos.is_empty() {
                "No repos configured.".to_string()
            } else {
                repos
                    .iter()
                    .map(human_repo_summary)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
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
) -> Result<RepoRecord> {
    if let Some(record) = store.get_repo_by_origin(origin)? {
        fs::create_dir_all(&record.workspace_root)?;
        materialize_repo_source_if_missing(runner, &record)?;
        return Ok(record);
    }

    let record = infer_repo_record_from_origin(origin, defaults)?;
    if let Some(existing) = store.get_repo(&record.repo)? {
        return Err(CubeError::InvalidArgument(format!(
            "repo `{}` is already configured for origin `{}`; cannot ensure `{origin}`",
            existing.repo, existing.origin
        )));
    }

    fs::create_dir_all(&record.workspace_root)?;
    materialize_repo_source_if_missing(runner, &record)?;
    store.upsert_repo(&record)
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
    fs::create_dir_all(parent)?;
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
    Ok(())
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
        WorkspaceCommand::Lease { repo, task } => {
            let repo_record = store
                .get_repo(&repo)?
                .ok_or_else(|| CubeError::RepoNotFound(repo.clone()))?;
            let _lock = RepoLock::acquire(&repo_lock_path(&repo, database_path)?)?;
            let mut candidates = discover_workspaces(&repo_record)?;
            store.sync_workspaces(&repo, &candidates)?;

            let lease_id = Uuid::new_v4().to_string();
            let holder = holder_identity();
            let leased_at_epoch_s = current_epoch_s()?;
            let mut workspace = match store.claim_workspace(
                &repo,
                &holder,
                &task,
                &lease_id,
                leased_at_epoch_s,
            )? {
                Some(ws) => ws,
                None => {
                    let new_candidate = auto_create_workspace(runner, &repo_record, &candidates)?;
                    candidates.push(new_candidate);
                    store.sync_workspaces(&repo, &candidates)?;
                    store
                        .claim_workspace(&repo, &holder, &task, &lease_id, leased_at_epoch_s)?
                        .ok_or_else(|| CubeError::NoAvailableWorkspace(repo.clone()))?
                }
            };

            if !workspace_path_exists(&workspace) {
                store.forget_workspace(&workspace.repo, &workspace.workspace_id)?;
                return Err(CubeError::NoAvailableWorkspace(repo));
            }

            if let Err(error) =
                reset_workspace(runner, &workspace.workspace_path, &repo_record.main_branch)
            {
                let _ = store.release_workspace(&lease_id);
                return Err(error);
            }

            let head_commit = current_workspace_commit(runner, &workspace.workspace_path)?;
            store.update_workspace_head_commit(&lease_id, Some(&head_commit))?;
            workspace.head_commit = Some(head_commit);

            RunResult::new(
                format!(
                    "Leased {} at {}.",
                    workspace.workspace_id,
                    workspace.workspace_path.display()
                ),
                json!({
                    "workspace": workspace,
                }),
            )
        }
        WorkspaceCommand::Release {
            workspace,
            lease,
            repo,
        } => {
            let lease = resolve_release_lease(&mut store, workspace, lease, repo)?;
            let workspace = store
                .get_workspace_by_lease(&lease)?
                .ok_or_else(|| CubeError::LeaseNotFound(lease.clone()))?;
            let _lock = RepoLock::acquire(&repo_lock_path(&workspace.repo, database_path)?)?;
            if !workspace_path_exists(&workspace) {
                store.forget_workspace(&workspace.repo, &workspace.workspace_id)?;
                return Err(CubeError::LeaseNotFound(lease));
            }
            let repo_record = store
                .get_repo(&workspace.repo)?
                .ok_or_else(|| CubeError::RepoNotFound(workspace.repo.clone()))?;
            reset_workspace(runner, &workspace.workspace_path, &repo_record.main_branch)?;
            let released = store
                .release_workspace(&lease)?
                .ok_or_else(|| CubeError::LeaseNotFound(lease.clone()))?;

            RunResult::new(
                format!("Released {}.", released.workspace_id),
                json!({
                    "workspace": released,
                }),
            )
        }
        WorkspaceCommand::Status { workspace } => {
            let path = PathBuf::from(&workspace);
            let record = find_workspace_record(&mut store, &path)?
                .ok_or_else(|| CubeError::WorkspaceNotFound(workspace.clone()))?;
            let jj_status = runner.run(&RealCommandRunner::invocation(&path, "jj", &["status"]))?;

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
            RunResult::new(
                format!("No setup steps are configured for {}.", record.workspace_id),
                json!({
                    "workspace": record,
                    "steps": [],
                }),
            )
        }
        WorkspaceCommand::List {
            repo,
            state,
            holder,
        } => {
            let parsed_state = match state.as_deref() {
                Some(raw) => Some(WorkspaceState::from_str(raw).ok_or_else(|| {
                    CubeError::InvalidArgument(format!(
                        "invalid --state `{raw}`; expected `free` or `leased`"
                    ))
                })?),
                None => None,
            };
            let filter = WorkspaceListFilter {
                repo: repo.as_deref(),
                state: parsed_state,
                holder_pattern: holder.as_deref(),
                ..Default::default()
            };
            let records = store.list_workspaces_filtered(&filter)?;
            let message = if records.is_empty() {
                "No workspaces match.".to_string()
            } else {
                records
                    .iter()
                    .map(human_workspace_summary)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            RunResult::new(
                message,
                json!({
                    "workspaces": records,
                }),
            )
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
                runner.run(&CommandInvocation {
                    cwd: workspace_path.clone(),
                    program: "jj".to_string(),
                    args: vec![
                        "new".to_string(),
                        parent.jj_change_id,
                        "-m".to_string(),
                        args.title.clone(),
                    ],
                })?;
            } else {
                runner.run(&CommandInvocation {
                    cwd: workspace_path.clone(),
                    program: "jj".to_string(),
                    args: vec!["describe".to_string(), "-m".to_string(), args.title.clone()],
                })?;
            }

            let identity = current_change_identity(runner, &workspace_path)?;
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

    fs::create_dir_all(&repo_record.workspace_root)?;

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

fn discover_workspaces(repo: &RepoRecord) -> Result<Vec<crate::metadata::WorkspaceCandidate>> {
    let mut candidates = Vec::new();
    if !repo.workspace_root.is_dir() {
        return Ok(candidates);
    }
    for entry in fs::read_dir(&repo.workspace_root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
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

fn reset_workspace(
    runner: &dyn CommandRunner,
    workspace_path: &Path,
    main_branch: &str,
) -> Result<()> {
    runner.run(&RealCommandRunner::invocation(
        workspace_path,
        "jj",
        &["git", "fetch"],
    ))?;
    runner.run(&RealCommandRunner::invocation(
        workspace_path,
        "jj",
        &["new", main_branch],
    ))?;
    Ok(())
}

fn current_workspace_commit(runner: &dyn CommandRunner, workspace_path: &Path) -> Result<String> {
    runner.run(&CommandInvocation {
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
    })
}

fn current_change_identity(
    runner: &dyn CommandRunner,
    workspace_path: &Path,
) -> Result<ChangeIdentity> {
    let output = runner.run(&CommandInvocation {
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
    })?;
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

fn human_workspace_summary(record: &WorkspaceRecord) -> String {
    let mut parts = vec![
        format!("{}/{}", record.repo, record.workspace_id),
        record.state.as_str().to_string(),
        record.workspace_path.display().to_string(),
    ];
    if let Some(holder) = &record.holder {
        parts.push(format!("holder={holder}"));
    }
    if let Some(task) = &record.task {
        parts.push(format!("task={task:?}"));
    }
    if let Some(lease_id) = &record.lease_id {
        parts.push(format!("lease={lease_id}"));
    }
    parts.join("  ")
}

fn human_workspace_detail(record: &crate::metadata::WorkspaceRecord, jj_status: &str) -> String {
    let mut lines = vec![
        format!("repo: {}", record.repo),
        format!("workspace_id: {}", record.workspace_id),
        format!("workspace_path: {}", record.workspace_path.display()),
        format!("state: {}", record.state.as_str()),
    ];
    if let Some(lease_id) = &record.lease_id {
        lines.push(format!("lease_id: {lease_id}"));
    }
    if let Some(holder) = &record.holder {
        lines.push(format!("holder: {holder}"));
    }
    if let Some(task) = &record.task {
        lines.push(format!("task: {task}"));
    }
    if let Some(head_commit) = &record.head_commit {
        lines.push(format!("head_commit: {head_commit}"));
    }
    lines.push("jj_status:".to_string());
    lines.push(jj_status.to_string());
    lines.join("\n")
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
        .map_err(std::io::Error::other)?
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

fn human_repo_summary(record: &RepoRecord) -> String {
    format!(
        "{}: {} ({}, prefix `{}`)",
        record.repo,
        record.workspace_root.display(),
        record.main_branch,
        record.workspace_prefix
    )
}

fn human_repo_detail(record: &RepoRecord) -> String {
    let mut lines = vec![
        format!("repo: {}", record.repo),
        format!("origin: {}", record.origin),
        format!("main_branch: {}", record.main_branch),
        format!("workspace_root: {}", record.workspace_root.display()),
        format!("workspace_prefix: {}", record.workspace_prefix),
    ];
    if let Some(source) = &record.source {
        lines.push(format!("source: {}", source.display()));
    }
    lines.join("\n")
}

fn human_change_detail(record: &ChangeRecord) -> String {
    let mut lines = vec![
        format!("change_id: {}", record.change_id),
        format!("repo: {}", record.repo),
        format!("workspace_path: {}", record.workspace_path.display()),
        format!("title: {}", record.title),
        format!("jj_change_id: {}", record.jj_change_id),
        format!("head_commit: {}", record.head_commit),
    ];
    if let Some(parent_change_id) = &record.parent_change_id {
        lines.push(format!("parent_change_id: {parent_change_id}"));
    }
    lines.push(format!("created_at_epoch_s: {}", record.created_at_epoch_s));
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

    use super::{CubeError, RepoEnsureDefaults, Result, run_with_context, run_with_dependencies};

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
        let result = run_with_context(ensure, Some(&database_path), &runner, Some(&defaults))
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
        let result = run_with_context(ensure, Some(&database_path), &runner, Some(&defaults))
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
        std::fs::create_dir_all(workspace_root.join("mono-agent-004")).expect("workspace dir");
        std::fs::create_dir_all(workspace_root.join("mono-agent-005")).expect("workspace dir");

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
        std::fs::create_dir_all(workspace_root.join("mono-agent-001")).expect("workspace dir");
        std::fs::create_dir_all(workspace_root.join("mono-agent-007")).expect("workspace dir");

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
        let lease = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "third",
        ]);
        let result = run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

        assert_eq!(
            result.payload["workspace"]["workspace_id"],
            "mono-agent-008"
        );
        runner.assert_exhausted();
    }

    #[test]
    fn next_workspace_id_picks_max_plus_one() {
        assert_eq!(super::next_workspace_id("mono-agent-", &[]), "mono-agent-001");
        assert_eq!(
            super::next_workspace_id(
                "mono-agent-",
                &[
                    "mono-agent-001".to_string(),
                    "mono-agent-002".to_string(),
                ],
            ),
            "mono-agent-003"
        );
        // Non-contiguous: jumps to max+1, doesn't fill the gap.
        assert_eq!(
            super::next_workspace_id(
                "mono-agent-",
                &[
                    "mono-agent-001".to_string(),
                    "mono-agent-007".to_string(),
                ],
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
    fn workspace_release_resets_and_frees_the_workspace() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004")).expect("workspace dir");

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
    fn workspace_release_by_workspace_id_resolves_active_lease() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004")).expect("workspace dir");

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
            "demo",
        ]);
        run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
        lease_runner.assert_exhausted();

        let release_runner = FakeRunner::new(vec![
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main"], ""),
        ]);
        let release =
            Cli::parse_from(["cube", "workspace", "release", "mono-agent-004"]);
        let result = run_with_dependencies(release, Some(&database_path), &release_runner)
            .expect("release by id");

        assert_eq!(result.payload["workspace"]["state"], "free");
        assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-004");
        release_runner.assert_exhausted();
    }

    #[test]
    fn workspace_release_by_workspace_id_errors_when_not_leased() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004")).expect("workspace dir");

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

        let release =
            Cli::parse_from(["cube", "workspace", "release", "mono-agent-004"]);
        let error =
            run_with_dependencies(release, Some(&database_path), &FakeRunner::default())
                .expect_err("release should fail");
        // Workspace id is unknown to the registry until something has synced
        // it, so this surfaces as WorkspaceNotFound.
        assert!(matches!(error, CubeError::WorkspaceNotFound(_)));
    }

    #[test]
    fn workspace_status_includes_jj_status_output() {
        let (tempdir, database_path) = with_database_path();
        let workspace_root = tempdir.path().join("workspaces");
        std::fs::create_dir_all(workspace_root.join("mono-agent-004")).expect("workspace dir");

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
        std::fs::create_dir_all(workspace_root.join("mono-agent-004")).expect("workspace dir");

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
        std::fs::create_dir_all(workspace_root.join("mono-agent-001")).expect("workspace dir");
        std::fs::create_dir_all(workspace_root.join("mono-agent-002")).expect("workspace dir");

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
            "demo",
        ]);
        run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

        // global list returns both rows
        let list_all = Cli::parse_from(["cube", "workspace", "list"]);
        let result_all =
            run_with_dependencies(list_all, Some(&database_path), &FakeRunner::default())
                .expect("list");
        let rows = result_all.payload["workspaces"]
            .as_array()
            .expect("array");
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
        std::fs::create_dir_all(workspace_root.join("mono-agent-004")).expect("workspace dir");

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
        std::fs::create_dir_all(workspace_root.join("mono-agent-004")).expect("workspace dir");

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
        std::fs::create_dir_all(workspace_root.join("mono-agent-004")).expect("workspace dir");

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
    }
}
