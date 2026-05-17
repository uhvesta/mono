//! The `HostAdapter` trait and `LocalHostAdapter` implementation.
//!
//! Phase 2 of the distributed-agent-execution design. Introduces
//! `HostAdapter` as the single abstraction that unifies workspace
//! lifecycle (cube lease/release/heartbeat) and worker spawn across
//! local and (in Phase 3) SSH-remote hosts.
//!
//! In Phase 2 only `LocalHostAdapter` is implemented; it wraps the
//! existing `CubeClient` + `ExecutionRunner` pair. `SshHostAdapter`
//! lands in Phase 3 without further changes to this interface.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::coordinator::{
    CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease,
    CubeWorkspaceStatus,
};
use crate::runner::{ExecutionRunner, RunOutcome};
use crate::work::{WorkExecution, WorkItem};

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
    ) -> Result<CubeWorkspaceLease> {
        self.cube_client
            .lease_workspace(repo_id, task, prefer_workspace_id)
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
