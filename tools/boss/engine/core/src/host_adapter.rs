//! The `HostAdapter` trait, `LocalHostAdapter`, and `SshHostAdapter`.
//!
//! Phase 2 introduced `HostAdapter` as the single abstraction that
//! unifies workspace lifecycle (cube lease/release/heartbeat) and
//! worker spawn across local and (Phase 3+) SSH-remote hosts. Phase 3
//! adds `SshHostAdapter`: workspace-lifecycle operations shell out to
//! `ssh <target> cube ... --json` over a persistent `ControlMaster`
//! connection, and the wrapper-distribution path keeps
//! `~/.boss-remote/bin/boss-remote-run` current on every host.
//!
//! `spawn_worker` for the SSH adapter is fully wired (dispatch-stack
//! PR 2): it ensures the wrapper is current, composes the worker prompt
//! via the shared `runner::compose_worker_spawn`, ships the worker's
//! `.claude` hook settings + initial prompt to the remote, opens the
//! reverse events-socket forward, and launches the detached worker — then
//! returns `WaitingHuman` so `completion::on_stop` drives the in_review /
//! PR-URL transition over the forwarded socket. Coordinator host-selection
//! / routing (PR 3) and live-status + transcript readback (PR 4) build on
//! top. The trait stays stable across local and remote.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::config::RuntimeConfig;
use crate::host_registry::Host;
use crate::coordinator::{
    CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease,
    CubeWorkspaceStatus,
};
use crate::remote_wrapper::remote_wrapper_path;
use crate::runner::{
    ComposedWorkerSpawn, ExecutionRunner, RunOutcome, RunWaitState, bazel_prepush_gate_text,
    compose_worker_spawn, work_item_name, work_item_task_kind,
};
use crate::ssh_spawn::{
    REASON_WORKER_LAUNCH_FAILED, RemoteSpawnPlan, perform_remote_launch, remote_events_socket_path,
};
use crate::ssh_transport::SshTransport;
use crate::work::{WorkDb, WorkExecution, WorkItem};
use crate::worker_setup::{WorkerKind, WorkerSetupInput, render_remote_settings_json};
use crate::wrapper_distribution::{WrapperPushLocks, WrapperPushOutcome, ensure_wrapper_current};

/// Remote dir (under `$HOME`) that holds rendered worker `--settings`
/// files. Outside any workspace tree so the worker's `jj`/`git` never
/// sees them — the same invariant the local runner keeps by writing
/// settings under the system temp dir.
const REMOTE_SETTINGS_DIR: &str = ".boss-remote/settings";

/// Remote shim binary name. Cube's standard install puts `boss-event` on
/// the worker's PATH (see the wrapper's note), so the hook command
/// resolves it by name rather than an engine-local absolute path.
const REMOTE_BOSS_EVENT_BIN: &str = "boss-event";

/// Abstracts all host-specific operations: workspace lifecycle and
/// worker spawn. Later phases extend this with control-channel
/// (probe/interrupt/stop) and event-socket/transcript-readback setup.
///
/// `host_id = "local"` is the special case for the coordinator's own
/// machine. Every other id corresponds to a registered SSH-reachable
/// remote (Phase 3+).
#[async_trait]
pub trait HostAdapter: Send + Sync {
    /// Stable host identifier (e.g. `"local"`, `"zakalwe"`).
    fn host_id(&self) -> &str;

    // ── Workspace lifecycle ─────────────────────────────────────────────────

    async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle>;

    async fn lease_workspace(
        &self,
        repo_id: &str,
        task: &str,
        prefer_workspace_id: Option<&str>,
        allow_dirty: bool,
        resume_pr: Option<u64>,
    ) -> Result<CubeWorkspaceLease>;

    async fn release_workspace(&self, lease_id: &str) -> Result<()>;

    async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()>;

    async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()>;

    async fn create_change(
        &self,
        workspace_path: &Path,
        title: &str,
    ) -> Result<CubeChangeHandle>;

    /// For `pr_review` executions: check out the PR head commit in the leased
    /// workspace instead of creating a fresh change. Returns the head SHA on
    /// success. Called only when the task's `pr_url` is non-empty; the normal
    /// `create_change` path is used otherwise.
    async fn checkout_pr_head_for_review(
        &self,
        workspace_path: &Path,
        pr_url: &str,
        repo_slug: &str,
    ) -> Result<String>;

    async fn workspace_status(&self, workspace_path: &Path) -> Result<CubeWorkspaceStatus>;

    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>>;

    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>>;

    /// Returns `(command_string, cwd)` for the subprocess that would be
    /// spawned with `args`. Used to populate dispatch-event `cube_command`
    /// fields for post-mortem diagnostics. Returns `None` for test doubles
    /// and remote adapters that don't use local subprocesses.
    fn command_repr(&self, args: &[&str]) -> Option<(String, String)>;

    // ── Worker spawn ────────────────────────────────────────────────────────

    /// Spawn a worker for the given execution in the leased workspace.
    ///
    /// For `LocalHostAdapter` this delegates to the `ExecutionRunner`
    /// (pane spawn via the macOS app). For `SshHostAdapter` (Phase 3)
    /// this drives the SSH-exec path with remote-forwarded event socket.
    async fn spawn_worker(
        &self,
        worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace_path: &Path,
        cube_change_id: Option<&str>,
    ) -> Result<RunOutcome>;

    // ── Live-status + transcript readback (Phase 4) ─────────────────────────

    /// Read up to `max_bytes` from the tail of the transcript at `path`
    /// on this host.
    ///
    /// `Ok(None)` means "the transcript is engine-local — read the path
    /// off the local filesystem". The default (and [`LocalHostAdapter`])
    /// returns `None` so the existing local read path is unchanged.
    /// [`SshHostAdapter`] overrides this to pull the byte suffix over its
    /// `ControlMaster` and returns `Ok(Some(jsonl))` — giving the
    /// transcript-tail RPC the same bytes a local run would have read.
    async fn read_transcript_tail_bytes(
        &self,
        _path: &str,
        _max_bytes: u64,
    ) -> Result<Option<String>> {
        Ok(None)
    }

    /// Re-establish the reverse events-socket forward for a detached run
    /// (`run_id` is the worker's `BOSS_RUN_ID` / execution id) after an
    /// engine restart, so its hook stream reaches `engine_events_socket`
    /// again.
    ///
    /// `Ok(false)` means "nothing to reattach": a local worker was a
    /// child of the previous engine process and is already gone, so the
    /// default (and [`LocalHostAdapter`]) is a no-op. [`SshHostAdapter`]
    /// overrides this to clear the stale remote socket and re-open the
    /// `ssh -R` forward, returning `Ok(true)` on success.
    async fn reattach_events_forward(
        &self,
        _run_id: &str,
        _engine_events_socket: &str,
    ) -> Result<bool> {
        Ok(false)
    }
}

// ── LocalHostAdapter ──────────────────────────────────────────────────────────

/// Local-host adapter: delegates workspace lifecycle to `CubeClient`
/// and worker spawn to `ExecutionRunner`. This is the only adapter
/// in Phase 2; it preserves existing local behavior exactly.
pub struct LocalHostAdapter {
    cube_client: Arc<dyn CubeClient>,
    execution_runner: Arc<dyn ExecutionRunner>,
}

impl LocalHostAdapter {
    pub fn new(
        cube_client: Arc<dyn CubeClient>,
        execution_runner: Arc<dyn ExecutionRunner>,
    ) -> Self {
        Self {
            cube_client,
            execution_runner,
        }
    }
}

#[async_trait]
impl HostAdapter for LocalHostAdapter {
    fn host_id(&self) -> &str {
        "local"
    }

    async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle> {
        self.cube_client.ensure_repo(origin).await
    }

    async fn lease_workspace(
        &self,
        repo_id: &str,
        task: &str,
        prefer_workspace_id: Option<&str>,
        allow_dirty: bool,
        resume_pr: Option<u64>,
    ) -> Result<CubeWorkspaceLease> {
        self.cube_client
            .lease_workspace(repo_id, task, prefer_workspace_id, allow_dirty, resume_pr)
            .await
    }

    async fn release_workspace(&self, lease_id: &str) -> Result<()> {
        self.cube_client.release_workspace(lease_id).await
    }

    async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()> {
        self.cube_client.heartbeat_lease(lease_id, ttl_seconds).await
    }

    async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()> {
        self.cube_client.force_release_lease(lease_id, reason).await
    }

    async fn create_change(
        &self,
        workspace_path: &Path,
        title: &str,
    ) -> Result<CubeChangeHandle> {
        self.cube_client.create_change(workspace_path, title).await
    }

    async fn checkout_pr_head_for_review(
        &self,
        workspace_path: &Path,
        pr_url: &str,
        repo_slug: &str,
    ) -> Result<String> {
        checkout_pr_head_local(workspace_path, pr_url, repo_slug).await
    }

    async fn workspace_status(&self, workspace_path: &Path) -> Result<CubeWorkspaceStatus> {
        self.cube_client.workspace_status(workspace_path).await
    }

    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        self.cube_client.list_workspaces().await
    }

    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        self.cube_client.list_repos().await
    }

    fn command_repr(&self, args: &[&str]) -> Option<(String, String)> {
        self.cube_client.command_repr(args)
    }

    async fn spawn_worker(
        &self,
        worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace_path: &Path,
        cube_change_id: Option<&str>,
    ) -> Result<RunOutcome> {
        self.execution_runner
            .run_execution(worker_id, execution, work_item, workspace_path, cube_change_id)
            .await
    }
}

/// Run `gh pr view` + `jj git fetch` + `jj edit` locally in `workspace_path`
/// to position the working copy at the PR head. Called by
/// [`LocalHostAdapter::checkout_pr_head_for_review`]; extracted as a free
/// function so the logic is readable without the `self` boilerplate.
async fn checkout_pr_head_local(
    workspace_path: &Path,
    pr_url: &str,
    repo_slug: &str,
) -> Result<String> {
    let pr_number = boss_github::pr_url::pr_number_from_url(pr_url)
        .ok_or_else(|| anyhow!("cannot parse PR number from URL: {pr_url}"))?;

    // 1. Fetch the current head SHA from GitHub via the shared gh-cli helper.
    let head_sha = git_utils::gh_cli::fetch_pr_head_oid(repo_slug, pr_number).await?;

    // 2. Fetch remote refs so jj knows about the head commit.
    {
        let output = Command::new("jj")
            .args(["git", "fetch"])
            .current_dir(workspace_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .context("failed to spawn `jj git fetch`")?;
        if !output.status.success() {
            return Err(anyhow!(
                "`jj git fetch` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }

    // 3. Move the workspace's working copy to the PR head.
    {
        let output = Command::new("jj")
            .args(["edit", &head_sha])
            .current_dir(workspace_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("failed to spawn `jj edit {head_sha}`"))?;
        if !output.status.success() {
            return Err(anyhow!(
                "`jj edit {head_sha}` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }

    Ok(head_sha)
}

// ── SshHostAdapter ────────────────────────────────────────────────────────────

/// SSH-reachable remote host adapter. Owns one [`SshTransport`] per
/// host and runs cube subcommands as `ssh <target> cube …` over the
/// persistent `ControlMaster` connection.
///
/// Workspace-lifecycle operations parse the same `cube … --json`
/// payloads as the local `CommandCubeClient`. `spawn_worker` ensures the
/// wrapper is current, composes the worker prompt via the shared path,
/// ships the worker's `.claude` settings + initial prompt to the remote,
/// opens the reverse events-socket forward, and launches the detached
/// remote worker — returning `WaitingHuman` so `completion::on_stop`
/// drives the in_review / PR-URL transition over the forwarded socket.
pub struct SshHostAdapter {
    transport: SshTransport,
    push_locks: WrapperPushLocks,
    /// Backing store for the shared prompt-composition path
    /// (`compose_worker_spawn`): parent-project / conflict / CI-attempt
    /// lookups, effort + model resolution.
    work_db: Arc<WorkDb>,
    /// Engine runtime config, injected for parity with `PaneSpawnRunner`
    /// and the dispatch-stack DI contract. Not yet read in this PR — the
    /// remote worker authenticates via the host's own out-of-band claude
    /// credentials and the model/effort knobs ride the prompt; cross-host
    /// model routing (PR3) and the live-status surface (PR4) consume it.
    #[allow(dead_code)]
    cfg: Arc<RuntimeConfig>,
    /// Absolute path of the engine's LOCAL events socket — the target of
    /// the reverse `ssh -R <remote sock>:<this>` forward, so the remote
    /// worker's hook events tunnel back to the same socket local workers
    /// write to.
    events_socket_path: PathBuf,
}

impl SshHostAdapter {
    pub fn new(
        transport: SshTransport,
        work_db: Arc<WorkDb>,
        cfg: Arc<RuntimeConfig>,
        events_socket_path: PathBuf,
    ) -> Self {
        Self {
            transport,
            push_locks: WrapperPushLocks::new(),
            work_db,
            cfg,
            events_socket_path,
        }
    }

    pub fn transport(&self) -> &SshTransport {
        &self.transport
    }

    /// Run a cube command on the remote and decode its `--json` output.
    /// Mirrors `CommandCubeClient::run_json` but routes through SSH.
    async fn run_cube_json(&self, args: &[&str]) -> Result<serde_json::Value> {
        let mut full: Vec<&str> = Vec::with_capacity(args.len() + 1);
        full.push("cube");
        full.extend_from_slice(args);
        let output = self.transport.run(&full).await?;
        if !output.success() {
            let detail = if !output.stderr.trim().is_empty() {
                output.stderr.trim().to_owned()
            } else if !output.stdout.trim().is_empty() {
                output.stdout.trim().to_owned()
            } else {
                format!("exit status {}", output.status)
            };
            return Err(anyhow!(
                "ssh cube command failed on host {}: {detail}",
                self.transport.host_id
            ));
        }
        serde_json::from_str(&output.stdout)
            .with_context(|| format!("decoding cube JSON output from host {}", self.transport.host_id))
    }

    /// Append the Bazel pre-push build gate to the worker prompt when the
    /// remote workspace is a Bazel workspace.
    ///
    /// The shared `compose_execution_prompt` injects this gate only when
    /// `is_bazel_workspace` matches — but that probes the *local*
    /// filesystem, and the workspace lives on the remote, so it never
    /// fires for a remote worker. Probe the remote for the marker files
    /// over the master and append the gate for the same execution kinds
    /// the local runner gates (`task_implementation` / `chore_implementation`).
    /// A probe failure logs and leaves the prompt unchanged rather than
    /// blocking the spawn.
    async fn append_remote_bazel_gate(
        &self,
        execution: &WorkExecution,
        workspace: &str,
        prompt_text: String,
    ) -> String {
        if !matches!(
            execution.kind.as_str(),
            "task_implementation" | "chore_implementation"
        ) {
            return prompt_text;
        }
        // Single-string command so the remote shell evaluates the whole
        // `test … -o …` expression (a multi-token argv would be
        // space-joined by ssh and mis-parsed). Workspace paths come from
        // cube and contain no shell metacharacters.
        let probe = format!(
            "test -f '{ws}/MODULE.bazel' -o -f '{ws}/WORKSPACE' -o -f '{ws}/WORKSPACE.bazel'",
            ws = workspace
        );
        match self.transport.run(&[probe.as_str()]).await {
            Ok(out) if out.success() => {
                let mut prompt_text = prompt_text;
                prompt_text.push_str(&bazel_prepush_gate_text());
                prompt_text
            }
            Ok(_) => prompt_text,
            Err(err) => {
                tracing::warn!(
                    host_id = %self.transport.host_id,
                    error = %format!("{err:#}"),
                    "remote bazel-marker probe failed; worker prompt omits the pre-push gate",
                );
                prompt_text
            }
        }
    }

    /// `mkdir -p` the parent dir on the remote, stage `contents` to a
    /// local temp file, and `scp` it to `remote_path`. `label` only feeds
    /// the staging filename + error context.
    async fn ship_file(
        &self,
        remote_dir: &str,
        remote_path: &str,
        contents: &str,
        label: &str,
    ) -> Result<()> {
        let host = &self.transport.host_id;
        let mkdir = self
            .transport
            .run(&["mkdir", "-p", remote_dir])
            .await
            .with_context(|| format!("mkdir {remote_dir} on host {host}"))?;
        if !mkdir.success() {
            bail!(
                "failed to create remote {label} dir {remote_dir} on host {host}: {}",
                non_empty(&mkdir.stderr, mkdir.status)
            );
        }
        let staged = stage_local_file(label, contents)
            .with_context(|| format!("staging remote {label} for host {host}"))?;
        let push = self
            .transport
            .scp_push(staged.path(), remote_path)
            .await
            .with_context(|| format!("scp {label} to host {host}"))?;
        if !push.success() {
            bail!(
                "failed to scp {label} to {remote_path} on host {host}: {}",
                non_empty(&push.stderr, push.status)
            );
        }
        Ok(())
    }
}

/// Prefer a command's trimmed stderr for a failure detail, falling back
/// to a synthetic `exit N` so the message is never empty.
fn non_empty(stderr: &str, status: i32) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        format!("exit {status}")
    } else {
        trimmed.to_owned()
    }
}

/// Write `contents` to a unique local staging file so `scp` has a real
/// on-disk path to push, returning an RAII guard that unlinks it on drop.
/// Mirrors `wrapper_distribution`'s staging pattern.
fn stage_local_file(label: &str, contents: &str) -> Result<StagedFile> {
    let dir = std::env::temp_dir();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!(
        "boss-remote-{label}.{}.{}.tmp",
        std::process::id(),
        nonce
    ));
    std::fs::write(&path, contents)
        .with_context(|| format!("writing staging file {path:?}"))?;
    Ok(StagedFile(path))
}

/// RAII guard that unlinks a local staging file on drop. Unlink errors
/// are swallowed — leaking a temp file is strictly better than masking
/// the real ship error.
struct StagedFile(PathBuf);

impl StagedFile {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[async_trait]
impl HostAdapter for SshHostAdapter {
    fn host_id(&self) -> &str {
        &self.transport.host_id
    }

    async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle> {
        #[derive(Deserialize)]
        struct RepoEnsurePayload {
            repo_id: String,
        }
        let payload: RepoEnsurePayload = serde_json::from_value(
            self.run_cube_json(&crate::repo_slug::repo_ensure_args(origin))
                .await?,
        )
        .context("decoding remote `cube repo ensure` payload")?;
        Ok(CubeRepoHandle {
            repo_id: payload.repo_id,
        })
    }

    async fn lease_workspace(
        &self,
        repo_id: &str,
        task: &str,
        prefer_workspace_id: Option<&str>,
        allow_dirty: bool,
        resume_pr: Option<u64>,
    ) -> Result<CubeWorkspaceLease> {
        #[derive(Deserialize)]
        struct LeasePayload {
            workspace: LeaseWorkspace,
        }
        #[derive(Deserialize)]
        struct LeaseWorkspace {
            lease_id: Option<String>,
            workspace_id: String,
            workspace_path: PathBuf,
        }
        let resume_pr_str = resume_pr.map(|n| n.to_string());
        let mut args: Vec<&str> = vec!["--json", "workspace", "lease", repo_id, "--task", task];
        if let Some(prefer) = prefer_workspace_id {
            args.extend_from_slice(&["--prefer", prefer]);
        }
        if allow_dirty {
            args.push("--allow-dirty");
        }
        if let Some(n) = resume_pr_str.as_deref() {
            args.extend_from_slice(&["--resume-pr", n]);
        }
        let payload: LeasePayload = serde_json::from_value(self.run_cube_json(&args).await?)
            .context("decoding remote `cube workspace lease` payload")?;
        let lease_id = payload
            .workspace
            .lease_id
            .context("remote cube workspace lease response missing lease_id")?;
        Ok(CubeWorkspaceLease {
            lease_id,
            workspace_id: payload.workspace.workspace_id,
            workspace_path: payload.workspace.workspace_path,
        })
    }

    async fn release_workspace(&self, lease_id: &str) -> Result<()> {
        let _ = self
            .run_cube_json(&["--json", "workspace", "release", "--lease", lease_id])
            .await?;
        Ok(())
    }

    async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()> {
        let ttl_string = ttl_seconds.map(|t| t.to_string());
        let mut args: Vec<&str> = vec!["--json", "workspace", "heartbeat", "--lease", lease_id];
        if let Some(ttl) = ttl_string.as_deref() {
            args.extend_from_slice(&["--ttl-seconds", ttl]);
        }
        let _ = self.run_cube_json(&args).await?;
        Ok(())
    }

    async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()> {
        let mut args: Vec<&str> =
            vec!["--json", "workspace", "force-release", "--lease", lease_id];
        if let Some(r) = reason {
            args.extend_from_slice(&["--reason", r]);
        }
        let _ = self.run_cube_json(&args).await?;
        Ok(())
    }

    async fn create_change(
        &self,
        workspace_path: &Path,
        title: &str,
    ) -> Result<CubeChangeHandle> {
        #[derive(Deserialize)]
        struct ChangePayload {
            change: ChangeRecord,
        }
        #[derive(Deserialize)]
        struct ChangeRecord {
            change_id: String,
        }
        let workspace_arg = workspace_path.display().to_string();
        let payload: ChangePayload = serde_json::from_value(
            self.run_cube_json(&[
                "--json",
                "change",
                "create",
                "--workspace",
                workspace_arg.as_str(),
                "--title",
                title,
            ])
            .await?,
        )
        .context("decoding remote `cube change create` payload")?;
        Ok(CubeChangeHandle {
            change_id: payload.change.change_id,
        })
    }

    async fn checkout_pr_head_for_review(
        &self,
        workspace_path: &Path,
        pr_url: &str,
        repo_slug: &str,
    ) -> Result<String> {
        let pr_number = boss_github::pr_url::pr_number_from_url(pr_url)
            .ok_or_else(|| anyhow!("cannot parse PR number from URL: {pr_url}"))?;
        let host = &self.transport.host_id;

        // 1. Fetch the head SHA from GitHub via the shared gh-cli helper.
        //    gh queries the GitHub API; the result is identical whether run
        //    locally or on the remote host, so we run it locally to avoid the
        //    SSH round-trip.
        let head_sha = git_utils::gh_cli::fetch_pr_head_oid(repo_slug, pr_number).await?;

        // 2. Fetch remote refs on the remote host.
        let workspace = workspace_path.display().to_string();
        let fetch_cmd = format!("cd '{}' && jj git fetch", workspace);
        let output = self
            .transport
            .run(&["sh", "-c", &fetch_cmd])
            .await
            .with_context(|| format!("failed to run `jj git fetch` on remote host {host}"))?;
        if !output.success() {
            return Err(anyhow!(
                "`jj git fetch` failed on {host}: {}",
                output.stderr.trim()
            ));
        }

        // 3. Move the working copy to the PR head on the remote host.
        let edit_cmd = format!("cd '{}' && jj edit '{head_sha}'", workspace);
        let output = self
            .transport
            .run(&["sh", "-c", &edit_cmd])
            .await
            .with_context(|| {
                format!("failed to run `jj edit {head_sha}` on remote host {host}")
            })?;
        if !output.success() {
            return Err(anyhow!(
                "`jj edit {head_sha}` failed on {host}: {}",
                output.stderr.trim()
            ));
        }

        Ok(head_sha)
    }

    async fn workspace_status(&self, workspace_path: &Path) -> Result<CubeWorkspaceStatus> {
        #[derive(Deserialize)]
        struct StatusPayload {
            workspace: StatusWorkspace,
        }
        #[derive(Deserialize)]
        struct StatusWorkspace {
            workspace_id: String,
            workspace_path: PathBuf,
            state: String,
            lease_id: Option<String>,
            holder: Option<String>,
            task: Option<String>,
            leased_at_epoch_s: Option<i64>,
            lease_expires_at_epoch_s: Option<i64>,
        }
        let workspace_arg = workspace_path.display().to_string();
        let payload: StatusPayload = serde_json::from_value(
            self.run_cube_json(&[
                "--json",
                "workspace",
                "status",
                "--workspace",
                workspace_arg.as_str(),
            ])
            .await?,
        )
        .context("decoding remote `cube workspace status` payload")?;
        Ok(CubeWorkspaceStatus {
            workspace_id: payload.workspace.workspace_id,
            workspace_path: payload.workspace.workspace_path,
            state: payload.workspace.state,
            lease_id: payload.workspace.lease_id,
            holder: payload.workspace.holder,
            task: payload.workspace.task,
            leased_at_epoch_s: payload.workspace.leased_at_epoch_s,
            lease_expires_at_epoch_s: payload.workspace.lease_expires_at_epoch_s,
        })
    }

    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        #[derive(Deserialize)]
        struct ListPayload {
            workspaces: Vec<ListWorkspace>,
        }
        #[derive(Deserialize)]
        struct ListWorkspace {
            workspace_id: String,
            workspace_path: PathBuf,
            state: String,
            lease_id: Option<String>,
            holder: Option<String>,
            task: Option<String>,
            leased_at_epoch_s: Option<i64>,
            lease_expires_at_epoch_s: Option<i64>,
        }
        let payload: ListPayload =
            serde_json::from_value(self.run_cube_json(&["--json", "workspace", "list"]).await?)
                .context("decoding remote `cube workspace list` payload")?;
        Ok(payload
            .workspaces
            .into_iter()
            .map(|w| CubeWorkspaceStatus {
                workspace_id: w.workspace_id,
                workspace_path: w.workspace_path,
                state: w.state,
                lease_id: w.lease_id,
                holder: w.holder,
                task: w.task,
                leased_at_epoch_s: w.leased_at_epoch_s,
                lease_expires_at_epoch_s: w.lease_expires_at_epoch_s,
            })
            .collect())
    }

    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        #[derive(Deserialize)]
        struct ListPayload {
            repos: Vec<ListRepo>,
        }
        #[derive(Deserialize)]
        struct ListRepo {
            repo: String,
            origin: String,
            main_branch: String,
            workspace_root: PathBuf,
            workspace_prefix: String,
            #[serde(default)]
            source: Option<PathBuf>,
        }
        let payload: ListPayload =
            serde_json::from_value(self.run_cube_json(&["--json", "repo", "list"]).await?)
                .context("decoding remote `cube repo list` payload")?;
        Ok(payload
            .repos
            .into_iter()
            .map(|r| CubeRepoSummary {
                repo_id: r.repo,
                origin: r.origin,
                main_branch: r.main_branch,
                workspace_root: r.workspace_root,
                workspace_prefix: r.workspace_prefix,
                source: r.source,
            })
            .collect())
    }

    fn command_repr(&self, args: &[&str]) -> Option<(String, String)> {
        let mut cmd = format!("ssh {}", self.transport.ssh_target);
        cmd.push(' ');
        cmd.push_str("cube");
        for a in args {
            cmd.push(' ');
            cmd.push_str(a);
        }
        // The "cwd" of a remote command is opaque from the engine's
        // side; surface the ssh target so post-mortems are reproducible.
        Some((cmd, format!("(remote: {})", self.transport.host_id)))
    }

    async fn spawn_worker(
        &self,
        _worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace_path: &Path,
        cube_change_id: Option<&str>,
    ) -> Result<RunOutcome> {
        let host = self.transport.host_id.clone();

        // 1. Verify the wrapper before any other work — drifted versions
        //    turn into `host_wrapper_push_failed` early so we don't try
        //    to invoke a stale wrapper contract.
        match ensure_wrapper_current(&self.transport, &self.push_locks).await? {
            WrapperPushOutcome::Ok => {}
            WrapperPushOutcome::Failed(_kind, detail) => {
                bail!("host_wrapper_push_failed on host {host}: {detail}");
            }
        }

        // 2. The coordinator leased the workspace + created the change on
        //    the remote before calling spawn; the lease id rides the
        //    execution row, and `workspace_path` is the REMOTE path.
        let lease_id = execution.cube_lease_id.clone().context(
            "execution missing cube_lease_id; coordinator must lease before remote spawn",
        )?;
        let run_id = execution.id.clone();
        let workspace = workspace_path.display().to_string();

        // 3. Compose the worker prompt + spawn config via the SHARED
        //    path so the remote worker gets a byte-identical brief to a
        //    local one (same task framing, branch name, acceptance
        //    criterion, effort addendum, product preamble).
        let ComposedWorkerSpawn {
            prompt_text,
            spawn_config,
        } = compose_worker_spawn(
            &self.work_db,
            _worker_id,
            execution,
            work_item,
            workspace_path,
            cube_change_id,
            // Editorial controls default OFF on the remote path: SshHostAdapter
            // does not hold a FeatureFlagsStore (its `cfg` is "not yet read";
            // see struct docs). The editorial kill switch defaults off, and the
            // feature only ever gated the local PaneSpawnRunner path, so passing
            // `false` preserves the original behavior. Wire feature flags into
            // the remote path alongside the cross-host config work (PR3/PR4).
            false,
            self.cfg.work.max_review_embed_diff_lines,
        )
        .await;
        // `compose_execution_prompt` decides the Bazel pre-push gate by
        // probing the LOCAL filesystem, which never matches a remote
        // workspace path — so probe the remote and append it ourselves.
        let prompt_text = self
            .append_remote_bazel_gate(execution, &workspace, prompt_text)
            .await;

        // 4. Render the remote worker settings: the same boss-event hooks
        //    as a local worker, but pointed at the FORWARDED events
        //    socket and the remote shim, and without the engine-data-dir
        //    sandbox (there is no Boss engine on the remote). Shipped
        //    outside the workspace tree and loaded via `--settings`,
        //    mirroring the local runner.
        let remote_socket = remote_events_socket_path(&run_id);
        let settings_input = WorkerSetupInput {
            run_id: run_id.clone(),
            lease_id: lease_id.clone(),
            workspace_path: PathBuf::from(&workspace),
            events_socket_path: PathBuf::from(&remote_socket),
            boss_event_path: PathBuf::from(REMOTE_BOSS_EVENT_BIN),
            draft_pr_mode: false,
            execution_kind: execution.kind.as_str().to_owned(),
            task_kind: work_item_task_kind(work_item).map(str::to_owned),
            worker_kind: WorkerKind::Standard,
        };
        let settings_json = render_remote_settings_json(&settings_input);

        // 5. Ship the prompt + settings to the remote. The prompt lives
        //    under `<workspace>/.boss/` (read by the wrapper via
        //    BOSS_INITIAL_INPUT_FILE); the settings live outside the tree
        //    under `~/.boss-remote/settings/`.
        let remote_prompt_dir = format!("{workspace}/.boss");
        let remote_prompt_path = format!("{remote_prompt_dir}/initial-input.txt");
        let remote_settings_dir = format!("~/{REMOTE_SETTINGS_DIR}");
        let remote_settings_path = format!("{remote_settings_dir}/{run_id}.json");
        self.ship_file(&remote_prompt_dir, &remote_prompt_path, &prompt_text, "prompt")
            .await?;
        self.ship_file(
            &remote_settings_dir,
            &remote_settings_path,
            &settings_json,
            "settings",
        )
        .await?;

        // 6. Open the reverse events tunnel and launch the detached
        //    remote worker (PR1 orchestration over the one master
        //    multiplex).
        let plan = RemoteSpawnPlan::builder()
            .run_id(run_id.clone())
            .lease_id(lease_id)
            .workspace_path(workspace)
            .maybe_repo_remote_url(
                (!execution.repo_remote_url.is_empty()).then(|| execution.repo_remote_url.clone()),
            )
            .events_socket_path(remote_socket)
            .initial_input_file(remote_prompt_path)
            .settings_file(remote_settings_path)
            .wrapper_path(remote_wrapper_path())
            .build();

        let engine_socket = self.events_socket_path.display().to_string();
        let outcome = perform_remote_launch(&self.transport, &plan, &engine_socket).await?;

        if !outcome.launched {
            let reason = outcome.failure_reason.unwrap_or(REASON_WORKER_LAUNCH_FAILED);
            let detail = outcome.detail.unwrap_or_default();
            bail!("{reason} on host {host}: {detail}");
        }

        // Persist the remote worker pid onto the run row so it is the
        // durable signal-addressing key the design's "Storage Additions"
        // calls for, and so a post-restart diagnostic can see which OS
        // process the detached worker is. Best-effort: a missing run row
        // (start_execution_run_on_host always inserts before spawn, so
        // this is only a race) or a write error logs and is swallowed —
        // the pid is informational, not a spawn precondition.
        if let Some(pid) = outcome.remote_pid {
            match self.work_db.set_run_remote_pid_for_execution(&run_id, pid) {
                Ok(true) => {}
                Ok(false) => tracing::warn!(
                    host_id = %host,
                    run_id = %run_id,
                    remote_pid = pid,
                    "remote spawn: no work_runs row to stamp remote_pid onto yet",
                ),
                Err(err) => tracing::warn!(
                    host_id = %host,
                    run_id = %run_id,
                    remote_pid = pid,
                    ?err,
                    "remote spawn: failed to persist remote_pid",
                ),
            }
        }

        tracing::info!(
            host_id = %host,
            run_id = %run_id,
            remote_pid = ?outcome.remote_pid,
            model = %spawn_config.model,
            "remote worker launched; awaiting Stop over the forwarded events socket",
        );

        // WaitingHuman: the lease + workspace are retained and
        // `completion::on_stop` drives the in_review / PR-URL transition
        // when the worker's Stop event tunnels back over the forwarded
        // socket (it keys purely on the run id we stamped into
        // BOSS_RUN_ID, so the completion path is transport-agnostic).
        // `slot_id` is `None` — a remote worker holds no local libghostty
        // pane slot, so the coordinator releases the worker-pool slot
        // inline. Cross-host pool accounting lands with routing in PR3.
        let pid_suffix = outcome
            .remote_pid
            .map(|p| format!(" (remote pid {p})"))
            .unwrap_or_default();
        Ok(RunOutcome {
            wait_state: RunWaitState::WaitingHuman,
            result_summary: Some(format!(
                "Launched remote worker '{}' on host {host}{pid_suffix}. \
                 Hook events tunnel back over the forwarded events socket.",
                work_item_name(work_item),
            )),
            attention: None,
            slot_id: None,
            spawn_config: Some(spawn_config),
        })
    }

    async fn read_transcript_tail_bytes(
        &self,
        path: &str,
        max_bytes: u64,
    ) -> Result<Option<String>> {
        // The recorded transcript_path is a path on the remote host's
        // filesystem; pull its byte suffix over the master so the RPC
        // sees the same JSONL a local read would.
        let content =
            crate::remote_transcript::pull_remote_transcript_tail(&self.transport, path, max_bytes)
                .await?;
        Ok(Some(content))
    }

    async fn reattach_events_forward(
        &self,
        run_id: &str,
        engine_events_socket: &str,
    ) -> Result<bool> {
        let remote_socket = remote_events_socket_path(run_id);
        let outcome = crate::ssh_spawn::reestablish_events_forward(
            &self.transport,
            &remote_socket,
            engine_events_socket,
        )
        .await?;
        if !outcome.launched {
            let detail = outcome.detail.unwrap_or_default();
            bail!(
                "reattach events forward failed on host {}: {detail}",
                self.transport.host_id
            );
        }
        tracing::info!(
            host_id = %self.transport.host_id,
            run_id = %run_id,
            remote_socket = %remote_socket,
            "reattach: re-established reverse events forward for detached remote run",
        );
        Ok(true)
    }
}

// ── HostAdapterProvider ─────────────────────────────────────────────────────────

/// Resolves the [`HostAdapter`] the coordinator should use for a host the
/// scheduler just selected. This is the seam that replaces the single
/// hardcoded `LocalHostAdapter` in the dispatch loop: the local host
/// returns the existing local adapter unchanged, and every other
/// (SSH-reachable) host gets an [`SshHostAdapter`] over a persistent
/// `ControlMaster` connection.
#[async_trait]
pub trait HostAdapterProvider: Send + Sync {
    /// Return (or lazily build) the adapter for `host`. Errors surface as
    /// a pre-start failure in the dispatch loop, leaving the execution
    /// recoverable on a later kick.
    async fn adapter_for(&self, host: &Host) -> Result<Arc<dyn HostAdapter>>;
}

/// The default provider: hands back the one local adapter for every host.
/// Used by tests and local-only deployments — when no remote hosts are
/// registered the scheduler only ever picks `local`, so the host argument
/// is irrelevant. Production swaps in [`SshHostAdapterProvider`].
pub struct LocalHostAdapterProvider {
    local: Arc<dyn HostAdapter>,
}

impl LocalHostAdapterProvider {
    pub fn new(local: Arc<dyn HostAdapter>) -> Self {
        Self { local }
    }
}

#[async_trait]
impl HostAdapterProvider for LocalHostAdapterProvider {
    async fn adapter_for(&self, _host: &Host) -> Result<Arc<dyn HostAdapter>> {
        Ok(Arc::clone(&self.local))
    }
}

/// Production provider: returns the local adapter for `host_id = "local"`
/// and builds (and caches) an [`SshHostAdapter`] for every other host.
///
/// Each remote host gets one `ControlMaster` connection, opened on first
/// use and reused for the engine's lifetime — matching the SSH-transport
/// lifecycle from PR1. The cache keys on host id; a stale entry from a
/// host that has since been disabled/removed is harmless because the
/// scheduler stops selecting it.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct SshHostAdapterProvider {
    /// The coordinator's own local adapter, returned verbatim for `local`.
    local: Arc<dyn HostAdapter>,
    /// Backing store for the shared prompt-composition path inside
    /// [`SshHostAdapter`].
    work_db: Arc<WorkDb>,
    /// Engine runtime config, threaded into each built `SshHostAdapter`
    /// for parity with the local `PaneSpawnRunner`.
    cfg: Arc<RuntimeConfig>,
    /// Absolute path of the engine's local events socket — the target of
    /// the per-run reverse `ssh -R` forward.
    events_socket_path: PathBuf,
    /// Engine-owned directory holding the per-host `ControlMaster` sockets.
    control_socket_dir: PathBuf,
    /// Lazily-built remote adapters, one per host id.
    #[builder(default = Mutex::new(HashMap::new()))]
    cache: Mutex<HashMap<String, Arc<dyn HostAdapter>>>,
}

impl SshHostAdapterProvider {
    pub fn new(
        local: Arc<dyn HostAdapter>,
        work_db: Arc<WorkDb>,
        cfg: Arc<RuntimeConfig>,
        events_socket_path: PathBuf,
        control_socket_dir: PathBuf,
    ) -> Self {
        Self {
            local,
            work_db,
            cfg,
            events_socket_path,
            control_socket_dir,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl HostAdapterProvider for SshHostAdapterProvider {
    async fn adapter_for(&self, host: &Host) -> Result<Arc<dyn HostAdapter>> {
        if host.id == "local" {
            return Ok(Arc::clone(&self.local));
        }

        let mut cache = self.cache.lock().await;
        if let Some(adapter) = cache.get(&host.id) {
            return Ok(Arc::clone(adapter));
        }

        let ssh_target = host.ssh_target.as_deref().with_context(|| {
            format!(
                "host '{}' has no ssh_target; cannot build an SSH adapter",
                host.id
            )
        })?;
        let transport = SshTransport::new(&host.id, ssh_target, &self.control_socket_dir);
        transport
            .open_control_master()
            .await
            .with_context(|| format!("opening ControlMaster to host '{}'", host.id))?;
        let adapter: Arc<dyn HostAdapter> = Arc::new(SshHostAdapter::new(
            transport,
            Arc::clone(&self.work_db),
            Arc::clone(&self.cfg),
            self.events_socket_path.clone(),
        ));
        cache.insert(host.id.clone(), Arc::clone(&adapter));
        Ok(adapter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Pure helpers ─────────────────────────────────────────────────────────

    #[test]
    fn non_empty_returns_trimmed_stderr_when_present() {
        assert_eq!(non_empty("boom", 1), "boom");
    }

    #[test]
    fn non_empty_trims_surrounding_whitespace() {
        assert_eq!(non_empty("  permission denied\n", 1), "permission denied");
    }

    #[test]
    fn non_empty_falls_back_to_synthetic_exit_for_empty_stderr() {
        assert_eq!(non_empty("", 2), "exit 2");
    }

    #[test]
    fn non_empty_falls_back_to_synthetic_exit_for_whitespace_only_stderr() {
        assert_eq!(non_empty("   \n\t ", 127), "exit 127");
    }

    #[test]
    fn stage_local_file_writes_contents_and_embeds_label() {
        let staged = stage_local_file("mylabel", "hello world").expect("staging file");

        // While the guard is live the file exists with the exact contents.
        assert!(staged.path().exists(), "staged file should exist on disk");
        assert_eq!(
            std::fs::read(staged.path()).expect("read staged file"),
            b"hello world",
        );

        // The filename embeds the caller-supplied label for diagnosability.
        let file_name = staged
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .expect("staged file has a UTF-8 name");
        assert!(
            file_name.contains("mylabel"),
            "filename {file_name:?} should embed the label",
        );
    }

    #[test]
    fn staged_file_is_removed_on_drop() {
        let path = {
            let staged = stage_local_file("dropme", "transient").expect("staging file");
            assert!(staged.path().exists());
            staged.path().to_path_buf()
        };
        // Guard has dropped: the on-disk file is unlinked.
        assert!(
            !path.exists(),
            "staging file should be removed once the guard drops",
        );
    }
}
