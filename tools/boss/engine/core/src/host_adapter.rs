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
//! `spawn_worker` for the SSH adapter is partially wired in this
//! phase: it ensures the wrapper is current and surfaces
//! `host_wrapper_push_failed` as a run-failure reason when push fails,
//! but the full SSH-forwarded events-socket + transcript-readback
//! pipeline is finished out under a follow-up tracked in the design
//! doc Phase 3 risks section. The trait stays stable across the two.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;

use crate::coordinator::{
    CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease,
    CubeWorkspaceStatus,
};
use crate::runner::{ExecutionRunner, RunOutcome};
use crate::ssh_transport::SshTransport;
use crate::work::{WorkExecution, WorkItem};
use crate::wrapper_distribution::{WrapperPushLocks, WrapperPushOutcome, ensure_wrapper_current};

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
    ) -> Result<CubeWorkspaceLease>;

    async fn release_workspace(&self, lease_id: &str) -> Result<()>;

    async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()>;

    async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()>;

    async fn create_change(
        &self,
        workspace_path: &PathBuf,
        title: &str,
    ) -> Result<CubeChangeHandle>;

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
    ) -> Result<CubeWorkspaceLease> {
        self.cube_client
            .lease_workspace(repo_id, task, prefer_workspace_id, allow_dirty)
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
        workspace_path: &PathBuf,
        title: &str,
    ) -> Result<CubeChangeHandle> {
        self.cube_client.create_change(workspace_path, title).await
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

// ── SshHostAdapter ────────────────────────────────────────────────────────────

/// SSH-reachable remote host adapter. Owns one [`SshTransport`] per
/// host and runs cube subcommands as `ssh <target> cube …` over the
/// persistent `ControlMaster` connection.
///
/// Workspace-lifecycle operations parse the same `cube … --json`
/// payloads as the local `CommandCubeClient`. `spawn_worker` ensures
/// the wrapper is current (re-pushing on version drift) and surfaces
/// `host_wrapper_push_failed` as a run-failure when the push fails;
/// the end-to-end remote-spawn pipeline (events-socket remote-forward,
/// transcript readback, signal channel) lands in a follow-up.
pub struct SshHostAdapter {
    transport: SshTransport,
    push_locks: WrapperPushLocks,
}

impl SshHostAdapter {
    pub fn new(transport: SshTransport) -> Self {
        Self {
            transport,
            push_locks: WrapperPushLocks::new(),
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
            self.run_cube_json(&["--json", "repo", "ensure", "--origin", origin])
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
        let mut args: Vec<&str> = vec!["--json", "workspace", "lease", repo_id, "--task", task];
        if let Some(prefer) = prefer_workspace_id {
            args.extend_from_slice(&["--prefer", prefer]);
        }
        if allow_dirty {
            args.push("--allow-dirty");
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
        workspace_path: &PathBuf,
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
        _execution: &WorkExecution,
        _work_item: &WorkItem,
        _workspace_path: &Path,
        _cube_change_id: Option<&str>,
    ) -> Result<RunOutcome> {
        // Verify the wrapper before any other work — drifted versions
        // turn into `host_wrapper_push_failed` early so we don't try
        // to invoke a stale wrapper contract.
        match ensure_wrapper_current(&self.transport, &self.push_locks).await? {
            WrapperPushOutcome::Ok => {}
            WrapperPushOutcome::Failed(_kind, detail) => {
                bail!(
                    "host_wrapper_push_failed on host {}: {detail}",
                    self.transport.host_id
                );
            }
        }
        // The remaining pipeline — opening the SSH-forwarded events
        // socket, exec'ing the wrapper, threading stdio back into the
        // engine's live-state surface, and wiring the
        // transcript-readback channel — is implementation-deferred to
        // a follow-up. The trait surface and the workspace-lifecycle
        // half land in this phase so the scheduler / wrapper
        // distribution / migration work can be merged independently.
        bail!(
            "SshHostAdapter::spawn_worker not yet wired end-to-end on host {}; \
             see Phase 3 PR description for the deferred-implementation note",
            self.transport.host_id
        )
    }
}
