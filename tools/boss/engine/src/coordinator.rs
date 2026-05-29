use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use boss_protocol::FrontendEvent;
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::config::RuntimeConfig;
use crate::conflict_diagnosis;
use crate::dispatch_events::{
    DispatchEvent, DispatchEventSink, NoopDispatchEventSink, Outcome as DispatchOutcome, Stage,
};
use crate::host_adapter::{HostAdapter, LocalHostAdapter};
use crate::metrics::Registry;
use crate::runner::{ExecutionRunner, RunOutcome};
use crate::work::{
    CreateAttentionItemInput, PreStartFailureOutcome, WorkDb, WorkExecution, WorkItem, WorkRun,
};

// Phase-3 counter handles for the cube workspace lease boundary.
crate::register_counter!(
    CUBE_WORKSPACE_LEASE_ATTEMPTS,
    "cube_workspace_lease.attempts",
    "Number of cube workspace lease invocations attempted (each fallback counts separately).",
);
crate::register_counter!(
    CUBE_WORKSPACE_LEASE_SUCCESS,
    "cube_workspace_lease.success",
    "Number of cube workspace lease invocations that succeeded.",
);
crate::register_counter!(
    CUBE_WORKSPACE_LEASE_FAILURE,
    "cube_workspace_lease.failure",
    "Number of cube workspace lease sequences that exhausted all attempts and failed.",
);

/// Register all cube-workspace-lease counter handles with `registry`. Called
/// from [`crate::metrics::init_all`] at engine startup.
pub fn register_metrics(registry: &Registry) {
    registry.register_counter(&CUBE_WORKSPACE_LEASE_ATTEMPTS);
    registry.register_counter(&CUBE_WORKSPACE_LEASE_SUCCESS);
    registry.register_counter(&CUBE_WORKSPACE_LEASE_FAILURE);
}

/// Hook invoked once per execution at the moment it transitions from
/// `ready` to `running` (`start_execution_run` succeeded). Production
/// wiring routes this into [`crate::completion::WorkerCompletionHandler::on_execution_started`],
/// which snapshots the bound chore PR's head SHA into
/// `work_executions.pr_head_before` for the Stop-boundary SHA-delta
/// gate. Decoupled from `WorkerCompletionHandler` directly so the
/// coordinator module doesn't take a hard dependency on the
/// completion module's surface.
#[async_trait]
pub trait ExecutionStartedHook: Send + Sync {
    async fn on_execution_started(&self, execution_id: &str);
}

/// No-op hook used as the default. Production swaps it out via
/// [`ExecutionCoordinator::set_execution_started_hook`].
#[derive(Debug, Default)]
pub struct NoopExecutionStartedHook;

#[async_trait]
impl ExecutionStartedHook for NoopExecutionStartedHook {
    async fn on_execution_started(&self, _execution_id: &str) {}
}

/// Hard cap on the worker pool. The runtime config can request a smaller
/// pool, but values above this are clamped (with a warning). The V2
/// design fixes 8 as the upper bound.
pub const MAX_WORKER_POOL_SIZE: usize = 8;

/// Upper bound on how long the engine waits for a single
/// `cube workspace lease` subprocess invocation before declaring the
/// attempt a timeout failure. The motivating incident
/// (`exec_18aec07893bd2e30_29`, 2026-05-12) sat in `worker_claimed/ok`
/// for ~46 seconds with no event because the cube subprocess never
/// returned and the engine was awaiting it unboundedly. With this
/// timeout the engine surfaces a `cube_workspace_lease_failed` event
/// and either falls back or fails cleanly within seconds.
const CUBE_LEASE_TIMEOUT: Duration = Duration::from_secs(30);

/// Same upper bound for `cube repo ensure`. `ensure_repo` is normally
/// fast (it's an idempotent record lookup), but the same hang class
/// applies if cube wedges, so we time-bound it too.
const CUBE_REPO_ENSURE_TIMEOUT: Duration = Duration::from_secs(60);

/// Backoff delays between successive pre-start retry attempts. Element N
/// is the sleep before attempt N+2 (the first retry, the second retry, …).
/// Three entries → up to 3 retries (4 total attempts) before a pre-start
/// failure surfaces to the operator.
const PRE_START_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(45),
];

/// How often `run_execution`'s [`HeartbeatGuard`] re-stamps the cube
/// lease expiry. Cube's `DEFAULT_LEASE_TTL_SECS` is 30 minutes, so a
/// 5-minute cadence gives ~6 chances to renew within one TTL window
/// — generous enough that a single failed beat (e.g., a transient
/// cube subprocess failure) doesn't immediately put the lease at
/// risk.
const LEASE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Owns the per-run cube lease heartbeat task. Dropping the guard
/// aborts the heartbeat — used at the end of `run_execution` so the
/// heartbeat cannot outlive its lease.
///
/// Background: cube treats any lease whose `lease_expires_at_epoch_s`
/// has passed as eligible for reclamation. Without periodic
/// heartbeats from the engine, every worker that runs longer than
/// the TTL is silently susceptible to having its workspace's `@`
/// reset by the next lease call. The investigation chore for
/// `mono-agent-001` (2026-05-12) traced Worf's "`@` got re-pointed
/// mid-flight" symptom to exactly this — the engine never called
/// `heartbeat_lease`, despite both cube and the trait defining it.
struct HeartbeatGuard {
    handle: tokio::task::JoinHandle<()>,
}

impl HeartbeatGuard {
    fn spawn(
        host_adapter: Arc<dyn HostAdapter>,
        lease_id: String,
        execution_id: String,
        run_id: String,
        worker_id: String,
    ) -> Self {
        Self::spawn_with_interval(
            host_adapter,
            lease_id,
            execution_id,
            run_id,
            worker_id,
            LEASE_HEARTBEAT_INTERVAL,
        )
    }

    /// Test seam: lets unit tests drive the heartbeat with a tiny
    /// interval (e.g., 50 ms) so they can exercise multiple beats
    /// without depending on tokio's paused-time API. Production
    /// callers go through [`Self::spawn`].
    fn spawn_with_interval(
        host_adapter: Arc<dyn HostAdapter>,
        lease_id: String,
        execution_id: String,
        run_id: String,
        worker_id: String,
        interval: Duration,
    ) -> Self {
        let handle = tokio::spawn(async move {
            // First tick fires immediately at start; the elapsed
            // interval is the *gap* between subsequent ticks. Skip
            // the first immediate tick so we don't issue a redundant
            // heartbeat the moment the lease was acquired.
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                match host_adapter.heartbeat_lease(&lease_id, None).await {
                    Ok(()) => {
                        tracing::debug!(
                            %execution_id,
                            %run_id,
                            %worker_id,
                            %lease_id,
                            "extended cube lease via heartbeat"
                        );
                    }
                    Err(err) => {
                        // A single failed heartbeat is not fatal — the
                        // lease still has up to a TTL of remaining
                        // life before cube will reclaim it. Log
                        // structured at WARN so an operator
                        // investigating a future "`@` moved" report
                        // can grep for failed beats and see the gap.
                        tracing::warn!(
                            %execution_id,
                            %run_id,
                            %worker_id,
                            %lease_id,
                            ?err,
                            "cube lease heartbeat failed; will retry next interval"
                        );
                    }
                }
            }
        });
        Self { handle }
    }
}

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CubeRepoHandle {
    pub repo_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CubeWorkspaceLease {
    pub lease_id: String,
    pub workspace_id: String,
    pub workspace_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CubeChangeHandle {
    pub change_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CubeWorkspaceStatus {
    pub workspace_id: String,
    pub workspace_path: PathBuf,
    pub state: String,
    pub lease_id: Option<String>,
    pub holder: Option<String>,
    pub task: Option<String>,
    pub leased_at_epoch_s: Option<i64>,
    pub lease_expires_at_epoch_s: Option<i64>,
}

/// Pool-config view of a repo as returned by `cube repo list --json`.
/// Used by the cold-pool probe in [`ExecutionCoordinator::schedule_execution`]
/// to decide whether the auto-provisioned defaults are worth flagging
/// to the operator. See `multi-repo-work-modeling.md` Q6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CubeRepoSummary {
    pub repo_id: String,
    pub origin: String,
    pub main_branch: String,
    pub workspace_root: PathBuf,
    pub workspace_prefix: String,
    pub source: Option<PathBuf>,
}

#[async_trait]
pub trait CubeClient: Send + Sync {
    async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle>;
    async fn lease_workspace(
        &self,
        repo_id: &str,
        task: &str,
        prefer_workspace_id: Option<&str>,
    ) -> Result<CubeWorkspaceLease>;
    async fn create_change(
        &self,
        workspace_path: &PathBuf,
        title: &str,
    ) -> Result<CubeChangeHandle>;
    async fn release_workspace(&self, lease_id: &str) -> Result<()>;
    async fn workspace_status(&self, workspace_path: &Path) -> Result<CubeWorkspaceStatus>;
    async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()>;
    async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()>;
    /// Snapshot every workspace cube knows about. Returns one entry
    /// per workspace, the same shape `workspace_status` returns for a
    /// single workspace.
    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>>;
    /// Snapshot every repo cube has registered. One round-trip;
    /// callers use it to inspect pool config for advisory checks like
    /// the cold-repo probe.
    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>>;
    /// Returns `(command_string, cwd)` for the subprocess that would be
    /// spawned with `args`. Used to populate `cube_command`/`cube_cwd`
    /// in dispatch events so failures are reproducible from the terminal.
    /// Returns `None` for test doubles that don't use real subprocesses.
    fn command_repr(&self, _args: &[&str]) -> Option<(String, String)> {
        None
    }
}

#[derive(Debug, Clone)]
pub struct CommandCubeClient {
    cfg: Arc<RuntimeConfig>,
}

fn shell_quote(arg: &str) -> String {
    if arg.is_empty() || arg.chars().any(|c| c.is_whitespace() || c == '"' || c == '\'') {
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        arg.to_owned()
    }
}

impl CommandCubeClient {
    pub fn new(cfg: Arc<RuntimeConfig>) -> Self {
        Self { cfg }
    }

    async fn run_json(&self, args: &[&str]) -> Result<serde_json::Value> {
        let agent = self.cfg.agent()?;
        let mut command = Command::new(&agent.cube.command);
        command
            .args(&agent.cube.args)
            .args(args)
            .current_dir(&self.cfg.work.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = command.output().await.with_context(|| {
            format!(
                "failed to spawn Cube command: {} {}",
                agent.cube.command,
                agent.cube.args.join(" ")
            )
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let detail = if !stderr.is_empty() {
                stderr
            } else if !stdout.is_empty() {
                stdout
            } else {
                format!("exit status {}", output.status)
            };
            return Err(anyhow!("Cube command failed: {detail}"));
        }

        serde_json::from_slice(&output.stdout).context("failed to decode Cube JSON output")
    }
}

#[async_trait]
impl CubeClient for CommandCubeClient {
    async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle> {
        #[derive(Deserialize)]
        struct RepoEnsurePayload {
            repo_id: String,
        }

        let payload: RepoEnsurePayload = serde_json::from_value(
            self.run_json(&["--json", "repo", "ensure", "--origin", origin])
                .await?,
        )
        .context("failed to decode `cube repo ensure` payload")?;
        Ok(CubeRepoHandle {
            repo_id: payload.repo_id,
        })
    }

    async fn lease_workspace(
        &self,
        repo_id: &str,
        task: &str,
        prefer_workspace_id: Option<&str>,
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

        let mut args: Vec<&str> = vec![
            "--json", "workspace", "lease", repo_id, "--task", task,
        ];
        if let Some(prefer) = prefer_workspace_id {
            args.extend_from_slice(&["--prefer", prefer]);
        }
        let payload: LeasePayload = serde_json::from_value(self.run_json(&args).await?)
            .context("failed to decode `cube workspace lease` payload")?;
        let lease_id = payload
            .workspace
            .lease_id
            .context("cube workspace lease response missing lease_id")?;
        Ok(CubeWorkspaceLease {
            lease_id,
            workspace_id: payload.workspace.workspace_id,
            workspace_path: payload.workspace.workspace_path,
        })
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

        let payload: ChangePayload = serde_json::from_value(
            self.run_json(&[
                "--json",
                "change",
                "create",
                "--workspace",
                &workspace_path.display().to_string(),
                "--title",
                title,
            ])
            .await?,
        )
        .context("failed to decode `cube change create` payload")?;
        Ok(CubeChangeHandle {
            change_id: payload.change.change_id,
        })
    }

    async fn release_workspace(&self, lease_id: &str) -> Result<()> {
        let _ = self
            .run_json(&["--json", "workspace", "release", "--lease", lease_id])
            .await?;
        Ok(())
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
            self.run_json(&["--json", "workspace", "status", "--workspace", &workspace_arg])
                .await?,
        )
        .context("failed to decode `cube workspace status` payload")?;
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

    async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()> {
        let ttl_string = ttl_seconds.map(|ttl| ttl.to_string());
        let mut args: Vec<&str> = vec!["--json", "workspace", "heartbeat", "--lease", lease_id];
        if let Some(ttl) = ttl_string.as_deref() {
            args.extend_from_slice(&["--ttl-seconds", ttl]);
        }
        let _ = self.run_json(&args).await?;
        Ok(())
    }

    async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()> {
        let mut args: Vec<&str> = vec!["--json", "workspace", "force-release", "--lease", lease_id];
        if let Some(reason) = reason {
            args.extend_from_slice(&["--reason", reason]);
        }
        let _ = self.run_json(&args).await?;
        Ok(())
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

        let payload: ListPayload = serde_json::from_value(
            self.run_json(&["--json", "workspace", "list"]).await?,
        )
        .context("failed to decode `cube workspace list` payload")?;
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
            serde_json::from_value(self.run_json(&["--json", "repo", "list"]).await?)
                .context("failed to decode `cube repo list` payload")?;
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
        let Ok(agent) = self.cfg.agent() else { return None };
        let cmd = std::iter::once(agent.cube.command.as_str())
            .chain(agent.cube.args.iter().map(String::as_str))
            .chain(args.iter().copied())
            .map(shell_quote)
            .collect::<Vec<_>>()
            .join(" ");
        let cwd = self.cfg.work.cwd.display().to_string();
        Some((cmd, cwd))
    }
}

#[derive(Debug, Clone)]
pub struct WorkerPool {
    inner: Arc<Mutex<WorkerPoolInner>>,
}

#[derive(Debug)]
struct WorkerPoolInner {
    workers: Vec<WorkerSlot>,
    /// Per-pool RNG used to pick a uniformly-random free worker when
    /// no workspace-affinity match is available. Seeded once at pool
    /// construction; advanced on each claim.
    rng: fastrand::Rng,
}

#[derive(Debug, Clone)]
struct WorkerSlot {
    worker_id: String,
    execution_id: Option<String>,
    last_workspace_id: Option<String>,
}

impl WorkerPool {
    pub fn new(size: usize) -> Self {
        let clamped = if size > MAX_WORKER_POOL_SIZE {
            tracing::warn!(
                requested = size,
                cap = MAX_WORKER_POOL_SIZE,
                "worker pool size exceeds hard cap; clamping"
            );
            MAX_WORKER_POOL_SIZE
        } else {
            size
        };
        let workers = (0..clamped)
            .map(|index| WorkerSlot {
                worker_id: format!("worker-{}", index + 1),
                execution_id: None,
                last_workspace_id: None,
            })
            .collect();
        Self {
            inner: Arc::new(Mutex::new(WorkerPoolInner {
                workers,
                rng: fastrand::Rng::new(),
            })),
        }
    }

    /// Claim an idle worker for `execution_id`. Selection is affinity-first:
    /// if `preferred_workspace_id` is set and an idle worker last ran in
    /// that workspace, that worker is chosen. Otherwise a free slot is
    /// picked uniformly at random — a cosmetic spread so we don't always
    /// hammer slot 1.
    pub async fn claim_worker(
        &self,
        execution_id: &str,
        preferred_workspace_id: Option<&str>,
    ) -> Option<String> {
        let mut inner = self.inner.lock().await;

        if let Some(target) = preferred_workspace_id {
            if let Some(idx) = inner.workers.iter().position(|w| {
                w.execution_id.is_none()
                    && w.last_workspace_id.as_deref() == Some(target)
            }) {
                let worker = &mut inner.workers[idx];
                worker.execution_id = Some(execution_id.to_owned());
                return Some(worker.worker_id.clone());
            }
        }

        let free: Vec<usize> = inner
            .workers
            .iter()
            .enumerate()
            .filter(|(_, w)| w.execution_id.is_none())
            .map(|(idx, _)| idx)
            .collect();
        let chosen_idx = *inner.rng.choice(&free)?;
        let worker = &mut inner.workers[chosen_idx];
        worker.execution_id = Some(execution_id.to_owned());
        Some(worker.worker_id.clone())
    }

    /// Skip-the-queue claim used by `bossctl agents launch`. Same
    /// affinity-then-random selection as `claim_worker`, but if every
    /// configured slot is busy and the pool is still below the hard
    /// cap (`MAX_WORKER_POOL_SIZE`) we grow the pool by one fresh slot
    /// and hand it back. Returns `None` only when the pool is already
    /// at the hard cap with no idle slot — at that point there's no
    /// pane the macOS app could render anyway, so the launch is
    /// rejected rather than silently overcommitting.
    pub async fn claim_worker_force(
        &self,
        execution_id: &str,
        preferred_workspace_id: Option<&str>,
    ) -> Option<String> {
        let mut inner = self.inner.lock().await;

        if let Some(target) = preferred_workspace_id {
            if let Some(idx) = inner.workers.iter().position(|w| {
                w.execution_id.is_none()
                    && w.last_workspace_id.as_deref() == Some(target)
            }) {
                let worker = &mut inner.workers[idx];
                worker.execution_id = Some(execution_id.to_owned());
                return Some(worker.worker_id.clone());
            }
        }

        let free: Vec<usize> = inner
            .workers
            .iter()
            .enumerate()
            .filter(|(_, w)| w.execution_id.is_none())
            .map(|(idx, _)| idx)
            .collect();
        if let Some(&idx) = inner.rng.choice(&free) {
            let worker = &mut inner.workers[idx];
            worker.execution_id = Some(execution_id.to_owned());
            return Some(worker.worker_id.clone());
        }

        // Every existing slot is busy. Grow the pool — bounded by the
        // hard cap so the app's 8-pane workspace can always render the
        // forced worker.
        if inner.workers.len() >= MAX_WORKER_POOL_SIZE {
            return None;
        }
        let new_index = inner.workers.len();
        let worker = WorkerSlot {
            worker_id: format!("worker-{}", new_index + 1),
            execution_id: Some(execution_id.to_owned()),
            last_workspace_id: None,
        };
        let id = worker.worker_id.clone();
        inner.workers.push(worker);
        Some(id)
    }

    /// Release `worker_id` back to the idle pool. If `last_workspace_id`
    /// is provided we record it as the worker's affinity for future
    /// preferred-workspace claims.
    pub async fn release_worker(&self, worker_id: &str, last_workspace_id: Option<&str>) {
        let mut inner = self.inner.lock().await;
        if let Some(worker) = inner
            .workers
            .iter_mut()
            .find(|worker| worker.worker_id == worker_id)
        {
            worker.execution_id = None;
            if let Some(workspace_id) = last_workspace_id {
                worker.last_workspace_id = Some(workspace_id.to_owned());
            }
        }
    }

    pub async fn capacity(&self) -> usize {
        let inner = self.inner.lock().await;
        inner.workers.len()
    }

    /// Return true if at least one worker slot is idle (not currently
    /// claimed by an in-flight execution). Used by the orphan-active
    /// sweep to bail early rather than touching the DB when no worker
    /// could pick up a newly-queued execution.
    pub async fn has_idle_worker(&self) -> bool {
        let inner = self.inner.lock().await;
        inner.workers.iter().any(|w| w.execution_id.is_none())
    }

    /// Return the set of execution ids currently claimed by a worker
    /// slot. Used by the orphan-active sweep as the `is_live` oracle:
    /// an execution that is not claimed has no live worker driving it
    /// even if its DB status is still non-terminal.
    pub async fn claimed_execution_ids(&self) -> std::collections::HashSet<String> {
        let inner = self.inner.lock().await;
        inner
            .workers
            .iter()
            .filter_map(|w| w.execution_id.clone())
            .collect()
    }

    /// Format a worker id for slot `slot_id`. Inverse of
    /// [`slot_id_from_worker_id`]; both sides of the
    /// engine-owns-allocation refactor lean on this string format
    /// being stable so `worker-{N}` and slot N stay 1:1.
    pub fn worker_id_for_slot(slot_id: u8) -> String {
        format!("worker-{}", slot_id)
    }

    #[cfg(test)]
    pub(crate) async fn idle_count(&self) -> usize {
        let inner = self.inner.lock().await;
        inner
            .workers
            .iter()
            .filter(|worker| worker.execution_id.is_none())
            .count()
    }

    #[cfg(test)]
    async fn worker_affinity(&self, worker_id: &str) -> Option<String> {
        let inner = self.inner.lock().await;
        inner
            .workers
            .iter()
            .find(|worker| worker.worker_id == worker_id)
            .and_then(|worker| worker.last_workspace_id.clone())
    }
}

/// Parse the trailing 1-indexed slot number out of a `worker-{N}`
/// id. Both numbers refer to the same physical pane (the engine
/// owns allocation; the app hosts the pane in slot N), so we
/// normalize on this single conversion at the engine→app boundary.
///
/// Returns `None` for ids that don't match the `worker-{N}` shape
/// or whose suffix isn't a positive `u8`. Callers should treat
/// `None` as a programming error — the only producer is
/// [`WorkerPool::claim_worker`].
pub fn slot_id_from_worker_id(worker_id: &str) -> Option<u8> {
    worker_id
        .strip_prefix("worker-")?
        .parse::<u8>()
        .ok()
        .filter(|n| *n >= 1)
}

/// Sink for `executions.<id>` topic invalidations. The engine wires this
/// to the topic broker; tests use a no-op or recording double.
#[async_trait]
pub trait ExecutionPublisher: Send + Sync {
    async fn publish(
        &self,
        execution_id: &str,
        work_item_id: &str,
        status: &str,
        reason: &str,
    );

    /// Publish a work-tree invalidation on the work item's product
    /// topic so subscribers (the kanban view) re-fetch and pick up
    /// status changes the coordinator drove from a non-request path
    /// — e.g., the auto-advance of `tasks.status` to `'active'` that
    /// happens inside `start_execution_run`.
    async fn publish_work_item_changed(
        &self,
        product_id: &str,
        work_item_id: &str,
        reason: &str,
    );

    /// Push a typed [`FrontendEvent`] verbatim on the work item's
    /// product topic. Used for activity-feed events such as
    /// `ConflictResolutionStarted` / `Succeeded` / `Failed` /
    /// `Abandoned` (design Q8) where subscribers need the full
    /// payload, not just a "refetch" hint.
    async fn publish_frontend_event_on_product(
        &self,
        product_id: &str,
        event: FrontendEvent,
    );

    /// Nudge the execution scheduler to drain its ready queue. Called
    /// by the merge-poller's conflict-detection path after inserting a
    /// `conflict_resolution` execution so the worker is dispatched
    /// promptly rather than waiting for the next opportunistic kick.
    /// Default is a no-op — only the production `BrokerExecutionPublisher`
    /// overrides this.
    fn kick_scheduler(&self) {}
}

#[derive(Default)]
pub struct NoopExecutionPublisher;

#[async_trait]
impl ExecutionPublisher for NoopExecutionPublisher {
    async fn publish(&self, _: &str, _: &str, _: &str, _: &str) {}
    async fn publish_work_item_changed(&self, _: &str, _: &str, _: &str) {}
    async fn publish_frontend_event_on_product(&self, _: &str, _: FrontendEvent) {}
}

/// Tiny abstraction so the coordinator can bump the shared work-revision
/// counter without depending on `ServerState`.
pub trait RevisionSource: Send + Sync {
    fn next(&self) -> u64;
}

impl RevisionSource for AtomicU64 {
    fn next(&self) -> u64 {
        self.fetch_add(1, Ordering::SeqCst) + 1
    }
}

pub struct ExecutionCoordinator {
    work_db: Arc<WorkDb>,
    worker_pool: WorkerPool,
    host_adapter: Arc<dyn HostAdapter>,
    publisher: Arc<dyn ExecutionPublisher>,
    /// Structured stream of dispatch-pipeline events. Defaults to a
    /// no-op so legacy tests and short-lived callers don't need to
    /// stand one up; production wiring should install a
    /// [`crate::dispatch_events::JsonlFileSink`] via
    /// [`ExecutionCoordinator::set_dispatch_events`] before
    /// scheduling starts.
    dispatch_events: Arc<dyn DispatchEventSink>,
    /// `true` while a `run_scheduler` task is alive. `kick()` returns
    /// without spawning when this is already set; the alive scheduler
    /// is responsible for noticing the wakeup via `scheduling_pending`.
    scheduling_active: AtomicBool,
    /// Wakeup flag set by every `kick()` (whether or not it spawned a
    /// fresh scheduler). The running scheduler reads + resets this on
    /// each outer iteration so that a kick which arrived during the
    /// drain — i.e. between the last `list_ready_executions()` call
    /// and the scheduler relinquishing `scheduling_active` — re-enters
    /// the drain loop instead of being silently dropped. Closes the
    /// TOCTOU between "queue saw empty" and "active=false" that left
    /// fresh `ready` executions stranded with no scheduler running.
    scheduling_pending: AtomicBool,
    /// Repo origin URLs the cold-pool probe has already inspected in
    /// this engine's lifetime. The probe runs once per URL on the
    /// first successful `ensure_repo` for that URL; subsequent
    /// dispatches against the same URL skip both the `cube repo list`
    /// round-trip and the attention-item write. Engine restart resets
    /// this; per `multi-repo-work-modeling.md` R4 the deduplication
    /// scope is engine-lifetime, not durable.
    repo_cold_probe_seen: Mutex<HashSet<String>>,
    /// Backoff delays between successive pre-start retry attempts.
    /// Defaults to [`PRE_START_RETRY_DELAYS`]. Tests may override via
    /// [`Self::with_pre_start_retry_delays`] to avoid real sleeps.
    pre_start_retry_delays: Vec<Duration>,
    /// Engine-wide counter registry. Defaults to a fresh local registry
    /// with the lease counters pre-registered so tests that do not call
    /// `set_metrics` still get valid increments. Production wires in the
    /// shared engine registry via `set_metrics` after construction.
    metrics: Arc<Registry>,
    /// Hook called when an execution transitions to `running`.
    /// Defaults to [`NoopExecutionStartedHook`]; production installs
    /// the `WorkerCompletionHandler` via
    /// [`Self::set_execution_started_hook`] so the SHA-delta gate
    /// can snapshot the bound chore PR's head SHA at run start.
    execution_started_hook: Arc<dyn ExecutionStartedHook>,
}

impl ExecutionCoordinator {
    /// Convenience constructor for tests and simple callers. Wraps the
    /// provided `cube_client` and `execution_runner` in a
    /// `LocalHostAdapter` and calls [`Self::with_publisher`].
    pub fn new(
        work_db: Arc<WorkDb>,
        worker_pool: WorkerPool,
        cube_client: Arc<dyn CubeClient>,
        execution_runner: Arc<dyn ExecutionRunner>,
    ) -> Self {
        let host_adapter = Arc::new(LocalHostAdapter::new(cube_client, execution_runner));
        Self::with_host_adapter_and_publisher(
            work_db,
            worker_pool,
            host_adapter,
            Arc::new(NoopExecutionPublisher::default()),
        )
    }

    /// Constructor that accepts a publisher alongside the cube/runner
    /// primitives. Wraps them in `LocalHostAdapter` and delegates to
    /// [`Self::with_host_adapter_and_publisher`].
    pub fn with_publisher(
        work_db: Arc<WorkDb>,
        worker_pool: WorkerPool,
        cube_client: Arc<dyn CubeClient>,
        execution_runner: Arc<dyn ExecutionRunner>,
        publisher: Arc<dyn ExecutionPublisher>,
    ) -> Self {
        let host_adapter = Arc::new(LocalHostAdapter::new(cube_client, execution_runner));
        Self::with_host_adapter_and_publisher(work_db, worker_pool, host_adapter, publisher)
    }

    /// Primary constructor for Phase 3+. Callers that need to dispatch
    /// to a non-local host (e.g. `SshHostAdapter`) build the adapter
    /// themselves and pass it here directly.
    pub fn with_host_adapter_and_publisher(
        work_db: Arc<WorkDb>,
        worker_pool: WorkerPool,
        host_adapter: Arc<dyn HostAdapter>,
        publisher: Arc<dyn ExecutionPublisher>,
    ) -> Self {
        // Build a local registry for tests that never call `set_metrics`.
        // Pre-register the lease counter handles so `.inc()` never panics
        // on "counter not registered" in a test context.
        let local_metrics = Arc::new(Registry::new());
        register_metrics(&local_metrics);
        Self {
            work_db,
            worker_pool,
            host_adapter,
            publisher,
            dispatch_events: Arc::new(NoopDispatchEventSink::default()),
            scheduling_active: AtomicBool::new(false),
            scheduling_pending: AtomicBool::new(false),
            repo_cold_probe_seen: Mutex::new(HashSet::new()),
            pre_start_retry_delays: PRE_START_RETRY_DELAYS.to_vec(),
            metrics: local_metrics,
            execution_started_hook: Arc::new(NoopExecutionStartedHook),
        }
    }

    /// Wire the execution-started hook. Production installs the
    /// `WorkerCompletionHandler` here so it can snapshot the bound
    /// chore PR's head SHA into `work_executions.pr_head_before`
    /// when an execution transitions to `running`.
    pub fn set_execution_started_hook(&mut self, hook: Arc<dyn ExecutionStartedHook>) {
        self.execution_started_hook = hook;
    }

    /// Wire the engine-global metrics registry into this coordinator.
    /// `app.rs` calls this once after `init_all` has registered the
    /// lease counter handles. Tests that omit this call use a pre-seeded
    /// local registry (created in `with_publisher`) so counter increments
    /// never panic.
    pub fn set_metrics(&mut self, metrics: Arc<Registry>) {
        self.metrics = metrics;
    }

    /// Override the pre-start retry delay schedule. Pass an empty vec
    /// to disable retries entirely (immediate permanent failure); pass
    /// short durations in tests to avoid real sleeps.
    pub fn with_pre_start_retry_delays(mut self, delays: Vec<Duration>) -> Self {
        self.pre_start_retry_delays = delays;
        self
    }

    /// Install a dispatch-event sink. The production engine threads
    /// in a `JsonlFileSink` writing under the Boss state root; tests
    /// pass a `RecordingDispatchEventSink` to assert on the stage
    /// timeline.
    pub fn set_dispatch_events(&mut self, sink: Arc<dyn DispatchEventSink>) {
        self.dispatch_events = sink;
    }

    /// Builder-style equivalent for callers that construct the
    /// coordinator inside an `Arc::new(...)` chain.
    pub fn with_dispatch_events(mut self, sink: Arc<dyn DispatchEventSink>) -> Self {
        self.dispatch_events = sink;
        self
    }

    pub fn worker_pool(&self) -> WorkerPool {
        self.worker_pool.clone()
    }

    pub fn kick(self: &Arc<Self>) {
        // Order matters: `scheduling_pending` must be written BEFORE we
        // contend on `scheduling_active`. If we lose the swap race
        // (another scheduler is already running) the alive scheduler
        // will read `scheduling_pending` after it drains and notice
        // the wakeup; if we win, the fresh scheduler will reset
        // pending on its way into the drain loop.
        self.scheduling_pending.store(true, Ordering::Release);
        if self.scheduling_active.swap(true, Ordering::AcqRel) {
            tracing::debug!(
                "scheduler_kick outcome=noop reason=already_running — wakeup latched via scheduling_pending"
            );
            return;
        }
        tracing::debug!("scheduler_kick outcome=spawn — starting new run_scheduler task");
        let coordinator = self.clone();
        tokio::spawn(async move {
            coordinator.run_scheduler().await;
        });
    }

    /// Spawn a background task that periodically wakes the scheduler and
    /// surfaces a warning when a `ready` execution has been sitting in
    /// the queue for longer than one heartbeat interval.
    ///
    /// Rationale. The dispatch happy path is: kanban drag → insert
    /// `ready` execution → [`kick`] → `run_scheduler` picks the row up
    /// and emits `request_recorded` within milliseconds. PR #345 closed
    /// the canonical kick/drain TOCTOU by latching every kick into
    /// [`scheduling_pending`], but a `ready` row that stalls at
    /// `status_transition` (no follow-up `request_recorded`) was seen
    /// in the wild — see `exec_18af3ba5259d32a8_12` (2026-05-13), which
    /// sat for 131s before the 90s-age orphan-active reconciler
    /// (PR #429) abandoned it and inserted a fresh redispatch.
    ///
    /// The heartbeat is a second line of defence, not a replacement for
    /// either mechanism:
    ///
    /// * It calls [`kick`] regardless of the in-memory active flag, so
    ///   any kick that was lost to a race the existing latching can't
    ///   cover is re-issued within one interval. The scheduler still
    ///   serializes drains through `scheduling_active`, so two
    ///   schedulers can never run concurrently.
    /// * When the heartbeat actually observes a stranded `ready` row
    ///   (anything older than the interval), it logs a `warn!` line
    ///   carrying the execution id so an operator sees the failure on
    ///   the first occurrence instead of waiting for the orphan
    ///   reconciler. "Fail loudly" was an explicit constraint of the
    ///   reporting work item.
    /// * PR #429's orphan-active reconciler stays intact: that path
    ///   handles the harder case where the execution row itself is
    ///   stale (worker dead, row claimed but not `ready`), which this
    ///   heartbeat does NOT address.
    pub fn spawn_scheduler_heartbeat(
        self: &Arc<Self>,
        interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let coordinator = self.clone();
        tokio::spawn(async move {
            // Stagger startup so the first beat doesn't race the
            // engine's own boot-time `kick()` (see `app.rs`).
            tokio::time::sleep(interval).await;
            let interval_ms = interval.as_millis() as u64;
            loop {
                let stranded = coordinator.stranded_ready_executions(interval_ms);
                if !stranded.is_empty() {
                    tracing::warn!(
                        count = stranded.len(),
                        oldest_age_ms = stranded
                            .iter()
                            .map(|(_, age_ms)| *age_ms)
                            .max()
                            .unwrap_or(0),
                        execution_ids = ?stranded
                            .iter()
                            .map(|(id, _)| id.as_str())
                            .collect::<Vec<_>>(),
                        "scheduler heartbeat: ready execution(s) older than \
                         the heartbeat interval found — kick/drain handoff \
                         may have dropped a wakeup; re-kicking now",
                    );
                }
                coordinator.kick();
                tokio::time::sleep(interval).await;
            }
        })
    }

    /// Return every `ready` execution whose `created_at` is older than
    /// `min_age_ms` milliseconds ago, paired with its age in
    /// milliseconds. Used by [`spawn_scheduler_heartbeat`] to surface
    /// stranded rows; kept as a separate method so the heartbeat path
    /// is testable without involving any timers.
    fn stranded_ready_executions(&self, min_age_ms: u64) -> Vec<(String, u64)> {
        let ready = match self.work_db.list_ready_executions() {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "scheduler heartbeat: failed to list ready executions; skipping pass",
                );
                return Vec::new();
            }
        };
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let cutoff_ms = min_age_ms;
        ready
            .into_iter()
            .filter_map(|exec| {
                let created_at_secs: u64 = exec.created_at.parse().ok()?;
                let age_ms = now_secs.saturating_sub(created_at_secs).saturating_mul(1000);
                if age_ms >= cutoff_ms {
                    Some((exec.id, age_ms))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Skip-the-queue dispatch for `bossctl agents launch`. Looks the
    /// execution up directly, claims a worker via
    /// `WorkerPool::claim_worker_force` (which grows the pool by one
    /// slot up to the hard cap when every configured slot is busy),
    /// and runs the same `schedule_execution` path the auto-dispatcher
    /// uses. Returns the worker id we landed on so callers can echo it
    /// back to the human.
    ///
    /// Errors when the execution is not in `ready` (already claimed by
    /// the auto-dispatcher in a race, terminal, or unknown), or when
    /// the worker pool is already at the hard cap with no idle slot.
    pub async fn force_dispatch(self: &Arc<Self>, execution_id: &str) -> Result<String> {
        let execution = self
            .work_db
            .get_execution(execution_id)
            .with_context(|| format!("failed to look up execution {execution_id}"))?;
        if execution.status != "ready" {
            return Err(anyhow!(
                "execution {execution_id} is in status {status:?}, not ready — cannot force-dispatch",
                status = execution.status,
            ));
        }
        let preferred_workspace_id = execution.preferred_workspace_id.clone();
        let worker_id = self
            .worker_pool
            .claim_worker_force(&execution.id, preferred_workspace_id.as_deref())
            .await
            .ok_or_else(|| {
                anyhow!(
                    "worker pool already at hard cap ({MAX_WORKER_POOL_SIZE}); cannot \
                     force-dispatch {execution_id}"
                )
            })?;
        if let Err(err) = self.schedule_execution(&execution, &worker_id).await {
            self.worker_pool
                .release_worker(&worker_id, preferred_workspace_id.as_deref())
                .await;
            return Err(err);
        }
        Ok(worker_id)
    }

    async fn run_scheduler(self: Arc<Self>) {
        // Lossless-wakeup loop. The `scheduling_pending` flag is reset
        // at the top of each iteration so we have a clean "have we
        // seen any new kicks since this drain started?" reading at
        // the bottom. The pattern handles three race classes:
        //
        //   1. Kick during drain: caught by the post-drain
        //      `scheduling_pending.load()` and re-enters the inner
        //      loop without releasing `scheduling_active`.
        //   2. Kick after we declared no-pending but before we set
        //      `scheduling_active=false`: the kicker observed active=true
        //      and noop'd, but our second `scheduling_pending.load()`
        //      (after active=false) picks it up and we re-acquire
        //      active to resume draining.
        //   3. Kick after we set `scheduling_active=false`: the kicker
        //      spawns a fresh scheduler; we observe that via the
        //      swap returning `true` and exit cleanly.
        //
        // Without this, the original `_guard`/`break` pattern lost
        // wakeups in the narrow window between "queue empty" and
        // "guard drops" — kicks landing in that window noop'd against
        // `scheduling_active=true` and the new `ready` row sat
        // forever with no scheduler running to pick it up. That is
        // the symptom motivating this fix (see `task_18ae9d21044843b8_44`).
        loop {
            self.scheduling_pending.store(false, Ordering::Release);
            let drain_outcome = self.drain_ready_queue().await;

            // Pool-exhaustion exits don't re-loop here: another
            // scheduler will spawn from the post-`release_worker`
            // `kick()`, and re-looping immediately would just hit the
            // same exhaustion. Fall through to the same active-release
            // logic — `scheduling_pending` may still have been set,
            // and respecting it lets a "fresh row arrived while we
            // were blocked on the pool" case re-attempt once a worker
            // is free without waiting for the next external event.
            let _ = drain_outcome;

            if self.scheduling_pending.load(Ordering::Acquire) {
                // A kick raced us during drain. Reset and re-drain
                // without giving up `scheduling_active`.
                continue;
            }

            // Relinquish the active flag. Any kick that lands from
            // here on will see `scheduling_active=false` on its swap
            // and spawn its own scheduler — but a kick that races
            // between this store and the post-store load below still
            // needs to be caught, hence the second check.
            self.scheduling_active.store(false, Ordering::Release);
            if !self.scheduling_pending.load(Ordering::Acquire) {
                return;
            }
            // A kick landed in the gap. Try to re-claim active; if
            // someone else (a freshly spawned scheduler) already has
            // it, they'll handle the drain.
            if self.scheduling_active.swap(true, Ordering::AcqRel) {
                return;
            }
            // We re-acquired; loop back to drain.
        }
    }

    /// Drain every currently-`ready` execution. Returns the reason the
    /// drain stopped so the caller can decide whether to re-enter
    /// immediately (queue empty + pending wakeup) or yield (pool
    /// exhausted).
    async fn drain_ready_queue(self: &Arc<Self>) -> DrainOutcome {
        loop {
            let Some(execution) = self.next_ready_execution() else {
                return DrainOutcome::QueueEmpty;
            };
            let preferred_workspace_id = execution.preferred_workspace_id.clone();

            // Stage 1: request_recorded — the execution row is ready
            // and the scheduler has picked it up. This event closes
            // the gap between "the row exists" and "the scheduler
            // saw it" that the motivating incident lived in.
            self.dispatch_events
                .emit(
                    DispatchEvent::new(Stage::RequestRecorded, DispatchOutcome::Ok, &execution.id)
                        .with_work_item(&execution.work_item_id)
                        .with_details(serde_json::json!({
                            "preferred_workspace_id": preferred_workspace_id,
                        })),
                )
                .await;
            tracing::info!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                preferred_workspace_id = ?preferred_workspace_id,
                "spawn_attempt status=ready -> picked_up"
            );

            let Some(worker_id) = self
                .worker_pool
                .claim_worker(&execution.id, preferred_workspace_id.as_deref())
                .await
            else {
                // Pool is fully claimed. The execution stays `ready`
                // and re-kicks when a worker is released; surface the
                // stall so an unexpectedly small pool is visible in
                // the engine log instead of failing silently.
                let pool_capacity = self.worker_pool.capacity().await;
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    pool_capacity,
                    "spawn_attempt status=ready -> deferred reason=pool_exhausted"
                );
                // Invariant: every `tasks.status = 'active'` chore
                // should be backed by a `running` execution / live
                // worker. If the pool stalled with active chores that
                // have no running execution, surface the gap so the
                // ghost-active state isn't silent — the human can
                // compare against `bossctl agents list`.
                let orphans = self
                    .work_db
                    .list_active_chores_without_live_run()
                    .unwrap_or_default();
                if !orphans.is_empty() {
                    tracing::warn!(
                        ghost_active = ?orphans,
                        pool_capacity,
                        "active chores without a running execution after pool exhaustion \
                         — `boss chore list --status active` and `bossctl agents list` will \
                         diverge until a slot frees up"
                    );
                }
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(
                            Stage::WorkerClaimed,
                            DispatchOutcome::Skipped,
                            &execution.id,
                        )
                        .with_work_item(&execution.work_item_id)
                        .with_details(serde_json::json!({
                            "reason": "pool_exhausted",
                            "pool_capacity": pool_capacity,
                            "ghost_active": orphans,
                        })),
                    )
                    .await;
                return DrainOutcome::PoolExhausted;
            };

            self.dispatch_events
                .emit(
                    DispatchEvent::new(Stage::WorkerClaimed, DispatchOutcome::Ok, &execution.id)
                        .with_work_item(&execution.work_item_id)
                        .with_worker(&worker_id),
                )
                .await;

            match self.schedule_execution(&execution, &worker_id).await {
                Ok(()) => {
                    tracing::info!(
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        worker_id = %worker_id,
                        "spawn_attempt status=ready -> spawned"
                    );
                }
                Err(err) => {
                    tracing::error!(
                        ?err,
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        worker_id = %worker_id,
                        "spawn_attempt status=ready -> failed reason=schedule_execution_error"
                    );
                    self.worker_pool
                        .release_worker(&worker_id, preferred_workspace_id.as_deref())
                        .await;
                }
            }
        }
    }

    fn next_ready_execution(&self) -> Option<WorkExecution> {
        match self.work_db.list_ready_executions() {
            Ok(mut executions) => executions.drain(..).next(),
            Err(err) => {
                tracing::error!(?err, "failed to list ready executions");
                None
            }
        }
    }

    async fn schedule_execution(
        self: &Arc<Self>,
        execution: &WorkExecution,
        worker_id: &str,
    ) -> Result<()> {
        // Double-spawn guard (Bug A): if another execution for this
        // work_item is already live (running or waiting_human), this
        // execution is a redundant duplicate created by the orphan sweep
        // racing with a still-active pane. Abandon it without spawning
        // so "execution run completed" doesn't fire prematurely.
        match self
            .work_db
            .get_live_execution_for_work_item(&execution.work_item_id, &execution.id)
        {
            Ok(Some(live)) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    live_execution_id = %live.id,
                    work_item_id = %execution.work_item_id,
                    "spawn_attempt: redundant — another execution is already live; deferring to that one",
                );
                if let Err(err) = self.work_db.mark_execution_redundant(&execution.id) {
                    tracing::error!(
                        execution_id = %execution.id,
                        ?err,
                        "spawn_attempt: failed to mark redundant execution abandoned",
                    );
                }
                return Err(anyhow::anyhow!(
                    "redundant spawn: execution {} for work_item {} superseded by live execution {}",
                    execution.id,
                    execution.work_item_id,
                    live.id,
                ));
            }
            Ok(None) => {}
            Err(err) => {
                // Non-fatal: if the DB check fails, proceed with the
                // spawn rather than blocking all dispatches. The worst
                // case is the double-spawn race we're trying to prevent,
                // which is the pre-existing behaviour.
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    ?err,
                    "spawn_attempt: live-execution check failed — proceeding without dedup guard",
                );
            }
        }

        let work_item = self
            .work_db
            .get_work_item(&execution.work_item_id)
            .with_context(|| format!("failed to resolve work item {}", execution.work_item_id))?;
        let task = execution_task_summary(execution, &work_item);

        let repo = match tokio::time::timeout(
            CUBE_REPO_ENSURE_TIMEOUT,
            self.host_adapter.ensure_repo(&execution.repo_remote_url),
        )
        .await
        {
            Ok(Ok(repo)) => repo,
            Ok(Err(err)) => {
                let ensure_repr = self.host_adapter.command_repr(&[
                    "--json", "repo", "ensure", "--origin", &execution.repo_remote_url,
                ]);
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(
                            Stage::CubeRepoEnsured,
                            DispatchOutcome::Error,
                            &execution.id,
                        )
                        .with_work_item(&execution.work_item_id)
                        .with_worker(worker_id)
                        .with_error(&err)
                        .with_cube_invocation(ensure_repr),
                    )
                    .await;
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    None,
                    "cube_repo_ensure_failed",
                    "Cube `repo ensure` failed",
                    &err,
                )?;
                return Err(err);
            }
            Err(_elapsed) => {
                let err = anyhow!(
                    "cube `repo ensure` timed out after {}s",
                    CUBE_REPO_ENSURE_TIMEOUT.as_secs()
                );
                let ensure_repr = self.host_adapter.command_repr(&[
                    "--json", "repo", "ensure", "--origin", &execution.repo_remote_url,
                ]);
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(
                            Stage::CubeRepoEnsured,
                            DispatchOutcome::Error,
                            &execution.id,
                        )
                        .with_work_item(&execution.work_item_id)
                        .with_worker(worker_id)
                        .with_error(&err)
                        .with_cube_invocation(ensure_repr)
                        .with_details(serde_json::json!({
                            "reason": "timeout",
                            "timeout_ms": CUBE_REPO_ENSURE_TIMEOUT.as_millis() as u64,
                        })),
                    )
                    .await;
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    None,
                    "cube_repo_ensure_failed",
                    "Cube `repo ensure` timed out",
                    &err,
                )?;
                return Err(err);
            }
        };
        self.maybe_probe_cold_repo(execution).await;
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::CubeRepoEnsured, DispatchOutcome::Ok, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_worker(worker_id)
                    .with_cube_repo(&repo.repo_id)
                    .with_cube_invocation(self.host_adapter.command_repr(&[
                        "--json", "repo", "ensure", "--origin", &execution.repo_remote_url,
                    ])),
            )
            .await;

        let lease = match self
            .lease_workspace_with_fallback(execution, worker_id, &repo, &task)
            .await
        {
            Ok(lease) => lease,
            Err(err) => {
                // The lease helper has already emitted attempt /
                // failure events for every try; convert the final
                // failure into the start-failure record so the
                // execution row flips to `failed` cleanly instead of
                // wedging in `worker_claimed`.
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    Some(repo.repo_id.as_str()),
                    "cube_workspace_lease_failed",
                    "Cube `workspace lease` failed",
                    &err,
                )?;
                return Err(err);
            }
        };
        {
            let mut lease_args = vec![
                "--json", "workspace", "lease", repo.repo_id.as_str(), "--task", task.as_str(),
            ];
            if let Some(p) = execution.preferred_workspace_id.as_deref() {
                lease_args.extend_from_slice(&["--prefer", p]);
            }
            self.dispatch_events
                .emit(
                    DispatchEvent::new(
                        Stage::CubeWorkspaceLeased,
                        DispatchOutcome::Ok,
                        &execution.id,
                    )
                    .with_work_item(&execution.work_item_id)
                    .with_worker(worker_id)
                    .with_cube_repo(&repo.repo_id)
                    .with_cube_lease(&lease.lease_id)
                    .with_cube_workspace(&lease.workspace_id)
                    .with_cube_invocation(self.host_adapter.command_repr(&lease_args)),
                )
                .await;
        }
        let change_title = execution_change_title(execution, &work_item);
        let workspace_path_str = lease.workspace_path.display().to_string();
        let change_repr: Option<(String, String)> = self.host_adapter.command_repr(&[
            "--json",
            "change",
            "create",
            "--workspace",
            &workspace_path_str,
            "--title",
            &change_title,
        ]);
        let change = match self
            .host_adapter
            .create_change(&lease.workspace_path, &change_title)
            .await
        {
            Ok(change) => change,
            Err(err) => {
                if let Err(release_err) = self.host_adapter.release_workspace(&lease.lease_id).await
                {
                    tracing::error!(
                        ?release_err,
                        lease_id = %lease.lease_id,
                        "failed to release workspace after change creation failure"
                    );
                }
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(
                            Stage::CubeChangeCreated,
                            DispatchOutcome::Error,
                            &execution.id,
                        )
                        .with_work_item(&execution.work_item_id)
                        .with_worker(worker_id)
                        .with_cube_repo(&repo.repo_id)
                        .with_cube_lease(&lease.lease_id)
                        .with_cube_workspace(&lease.workspace_id)
                        .with_error(&err)
                        .with_cube_invocation(change_repr.clone()),
                    )
                    .await;
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    Some(repo.repo_id.as_str()),
                    "cube_change_create_failed",
                    "Cube `change create` failed",
                    &err,
                )?;
                return Err(err);
            }
        };
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::CubeChangeCreated, DispatchOutcome::Ok, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_worker(worker_id)
                    .with_cube_repo(&repo.repo_id)
                    .with_cube_lease(&lease.lease_id)
                    .with_cube_workspace(&lease.workspace_id)
                    .with_cube_invocation(change_repr)
                    .with_details(serde_json::json!({
                        "change_id": change.change_id,
                        "change_title": change_title,
                    })),
            )
            .await;

        match self.work_db.start_execution_run(
            &execution.id,
            worker_id,
            &repo.repo_id,
            &lease.lease_id,
            &lease.workspace_id,
            &lease.workspace_path.display().to_string(),
        ) {
            Ok((execution, run)) => {
                let worker_id_owned = worker_id.to_owned();
                tracing::info!(
                    execution_id = %execution.id,
                    run_id = %run.id,
                    worker_id,
                    cube_repo_id = %repo.repo_id,
                    cube_lease_id = %lease.lease_id,
                    cube_workspace_id = %lease.workspace_id,
                    cube_change_id = %change.change_id,
                    workspace_path = %lease.workspace_path.display(),
                    "started execution run"
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::RunStarted, DispatchOutcome::Ok, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(worker_id)
                            .with_cube_repo(&repo.repo_id)
                            .with_cube_lease(&lease.lease_id)
                            .with_cube_workspace(&lease.workspace_id)
                            .with_details(serde_json::json!({
                                "run_id": run.id,
                            })),
                    )
                    .await;
                self.publisher
                    .publish(
                        &execution.id,
                        &execution.work_item_id,
                        &execution.status,
                        "execution_started",
                    )
                    .await;
                // Auto-advance bumped `tasks.status` to `'active'`
                // inside the same transaction. Broadcast a work-tree
                // invalidation so kanban subscribers re-fetch and
                // move the card to the Doing column.
                if let Ok(work_item) = self.work_db.get_work_item(&execution.work_item_id) {
                    self.publisher
                        .publish_work_item_changed(
                            &work_item_product_id(&work_item),
                            &execution.work_item_id,
                            "execution_started_auto_advance",
                        )
                        .await;
                }
                // Resume-bounce SHA-delta gate: capture the bound
                // chore PR's head SHA into the execution row BEFORE
                // the worker spawns and starts pushing. The Stop
                // boundary uses this snapshot to decide whether the
                // run contributed to the bound PR. Best-effort: the
                // hook logs and swallows every failure mode (no
                // bound PR, slug/number parse failure, GitHub fetch
                // failure), and the gate treats a missing snapshot
                // as "inapplicable" — never noisier than the
                // pre-change behaviour.
                self.execution_started_hook
                    .on_execution_started(&execution.id)
                    .await;
                let coordinator = self.clone();
                tokio::spawn(async move {
                    coordinator
                        .run_execution(execution, run, work_item, worker_id_owned, lease, change)
                        .await;
                });
                Ok(())
            }
            Err(err) => {
                let release_result = self.host_adapter.release_workspace(&lease.lease_id).await;
                if let Err(release_err) = release_result {
                    tracing::error!(
                        ?release_err,
                        lease_id = %lease.lease_id,
                        "failed to release workspace after run start failure"
                    );
                }
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(
                            Stage::RunStarted,
                            DispatchOutcome::Error,
                            &execution.id,
                        )
                        .with_work_item(&execution.work_item_id)
                        .with_worker(worker_id)
                        .with_cube_repo(&repo.repo_id)
                        .with_cube_lease(&lease.lease_id)
                        .with_cube_workspace(&lease.workspace_id)
                        .with_error(&err),
                    )
                    .await;
                self.record_start_failure(
                    Arc::clone(self),
                    execution,
                    worker_id,
                    Some(repo.repo_id.as_str()),
                    "execution_run_start_failed",
                    "`start_execution_run` failed",
                    &err,
                )?;
                Err(err)
            }
        }
    }

    /// Cold-repo probe (design doc Q6, Follow-up chore #8). The first
    /// time a given repo URL flows through `ensure_repo` in this
    /// engine's lifetime, ask cube `repo list --json` once and check
    /// whether the entry for this URL is sitting on cube's
    /// auto-provisioned defaults — i.e. nothing was customised with
    /// `cube repo add` / `cube repo configure`. If so, raise an
    /// advisory `repo_cold_pool` `WorkAttentionItem` against the
    /// execution naming the exact override command.
    ///
    /// Best-effort by design: never blocks dispatch, never returns an
    /// error to the caller. A failed `list_repos` round-trip is logged
    /// at WARN and the URL is still marked seen so we don't retry the
    /// probe every dispatch — engine restart re-probes per R4.
    async fn maybe_probe_cold_repo(self: &Arc<Self>, execution: &WorkExecution) {
        let origin = execution.repo_remote_url.clone();
        {
            let mut seen = self.repo_cold_probe_seen.lock().await;
            if !seen.insert(origin.clone()) {
                return;
            }
        }

        let repos = match self.host_adapter.list_repos().await {
            Ok(repos) => repos,
            Err(err) => {
                tracing::warn!(
                    ?err,
                    execution_id = %execution.id,
                    repo_remote_url = %origin,
                    "cold-repo probe: `cube repo list` failed — skipping advisory check"
                );
                return;
            }
        };

        let Some(repo) = repos.iter().find(|r| r.origin == origin) else {
            tracing::debug!(
                execution_id = %execution.id,
                repo_remote_url = %origin,
                "cold-repo probe: ensured repo not present in `cube repo list` snapshot"
            );
            return;
        };

        if !repo_has_default_pool_config(repo) {
            return;
        }

        let title = format!(
            "Cold cube pool for `{repo_id}` — using auto-provisioned defaults",
            repo_id = repo.repo_id,
        );
        let body = cold_repo_attention_body(repo);
        let input = CreateAttentionItemInput {
            execution_id: Some(execution.id.clone()),
            work_item_id: None,
            kind: "repo_cold_pool".to_owned(),
            status: None,
            title,
            body_markdown: body,
            resolved_at: None,
        };
        match self.work_db.create_attention_item(input) {
            Ok(item) => {
                tracing::info!(
                    attention_id = %item.id,
                    execution_id = %execution.id,
                    repo_id = %repo.repo_id,
                    repo_remote_url = %origin,
                    "cold-repo probe: raised advisory attention item"
                );
            }
            Err(err) => {
                tracing::warn!(
                    ?err,
                    execution_id = %execution.id,
                    repo_id = %repo.repo_id,
                    repo_remote_url = %origin,
                    "cold-repo probe: failed to persist attention item — dispatch continues"
                );
            }
        }
    }

    /// Lease a cube workspace for `execution`, emitting a structured
    /// attempt/failure event for every try and falling back to "any
    /// free workspace" when an unprefixed lease fails.
    ///
    /// Behaviour matrix:
    ///
    /// | preferred set? | first attempt      | on first failure                          |
    /// |----------------|--------------------|-------------------------------------------|
    /// | no             | without `--prefer` | retry once without `--prefer` (`any_free`) |
    /// | yes            | with `--prefer`    | terminal failure (preserves continuity)   |
    ///
    /// When `preferred_workspace_id` is set the caller needs a specific
    /// workspace (e.g. resuming a prior run). Silently landing elsewhere
    /// would lose state continuity, so we fail fast and let the scheduler
    /// retry the dispatch later. When no preference is set any free
    /// workspace is acceptable, so a single bad workspace cannot block
    /// the entire dispatch.
    ///
    /// Each subprocess invocation is bounded by [`CUBE_LEASE_TIMEOUT`]
    /// so the engine cannot wedge indefinitely waiting on cube — the
    /// motivating incident sat in `worker_claimed/ok` for ~46s with
    /// no event because the cube call never returned.
    async fn lease_workspace_with_fallback(
        &self,
        execution: &WorkExecution,
        worker_id: &str,
        repo: &CubeRepoHandle,
        task: &str,
    ) -> Result<CubeWorkspaceLease> {
        let prefer = execution.preferred_workspace_id.as_deref();
        // Soft-prefer (OQ5): revision_implementation executions set
        // prefer_is_soft = true so a missing or leased preferred workspace
        // degrades silently to any free workspace rather than failing hard.
        // Orphan-resume executions use the hard "none" policy (prefer_is_soft
        // = false) because their state lives only in that specific workspace.
        let fallback_policy = if prefer.is_none() || execution.prefer_is_soft {
            "any_free"
        } else {
            "none"
        };

        // Build the lease args for attempt 1 so we can attach the
        // exact command to both the attempted and failed events.
        let mut attempt1_args = vec![
            "--json", "workspace", "lease", repo.repo_id.as_str(), "--task", task,
        ];
        if let Some(p) = prefer {
            attempt1_args.extend_from_slice(&["--prefer", p]);
        }
        let attempt1_repr = self.host_adapter.command_repr(&attempt1_args);

        // First attempt: use the preferred workspace if the caller
        // pinned one. Emit `cube_workspace_lease_attempted` *before*
        // the subprocess so the timeline shows what we tried even
        // when cube hangs and never returns.
        self.dispatch_events
            .emit(
                DispatchEvent::new(
                    Stage::CubeWorkspaceLeaseAttempted,
                    DispatchOutcome::Ok,
                    &execution.id,
                )
                .with_work_item(&execution.work_item_id)
                .with_worker(worker_id)
                .with_cube_repo(&repo.repo_id)
                .with_cube_invocation(attempt1_repr.clone())
                .with_details(serde_json::json!({
                    "attempt": 1,
                    "prefer_workspace_id": prefer,
                    "fallback_policy": fallback_policy,
                    "timeout_ms": CUBE_LEASE_TIMEOUT.as_millis() as u64,
                })),
            )
            .await;

        CUBE_WORKSPACE_LEASE_ATTEMPTS.inc(&self.metrics);
        let first_err = match self
            .invoke_lease(repo, task, prefer, CUBE_LEASE_TIMEOUT)
            .await
        {
            Ok(lease) => {
                CUBE_WORKSPACE_LEASE_SUCCESS.inc(&self.metrics);
                return Ok(lease);
            }
            Err((reason, err)) => {
                tracing::error!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    worker_id,
                    cube_repo_id = %repo.repo_id,
                    prefer = ?prefer,
                    reason,
                    error = format!("{err:#}"),
                    "cube workspace lease attempt failed"
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(
                            Stage::CubeWorkspaceLeaseFailed,
                            DispatchOutcome::Error,
                            &execution.id,
                        )
                        .with_work_item(&execution.work_item_id)
                        .with_worker(worker_id)
                        .with_cube_repo(&repo.repo_id)
                        .with_error(&err)
                        .with_cube_invocation(attempt1_repr)
                        .with_details(serde_json::json!({
                            "attempt": 1,
                            "prefer_workspace_id": prefer,
                            "reason": reason,
                            "fallback_policy": fallback_policy,
                        })),
                    )
                    .await;
                err
            }
        };

        // Fallback only kicks in when the first attempt had no workspace
        // preference, OR when prefer_is_soft is true (revision_implementation
        // uses a soft prefer for cache warmth only — losing the preferred
        // workspace is a non-event, not a continuity failure).
        // With a hard prefer (prefer set + prefer_is_soft = false), the
        // caller needs that specific workspace (orphan-resume); silently
        // landing elsewhere would lose local commit state.
        if prefer.is_some() && !execution.prefer_is_soft {
            CUBE_WORKSPACE_LEASE_FAILURE.inc(&self.metrics);
            return Err(first_err);
        }

        let attempt2_args = vec![
            "--json", "workspace", "lease", repo.repo_id.as_str(), "--task", task,
        ];
        let attempt2_repr = self.host_adapter.command_repr(&attempt2_args);

        self.dispatch_events
            .emit(
                DispatchEvent::new(
                    Stage::CubeWorkspaceLeaseAttempted,
                    DispatchOutcome::Ok,
                    &execution.id,
                )
                .with_work_item(&execution.work_item_id)
                .with_worker(worker_id)
                .with_cube_repo(&repo.repo_id)
                .with_cube_invocation(attempt2_repr.clone())
                .with_details(serde_json::json!({
                    "attempt": 2,
                    "prefer_workspace_id": serde_json::Value::Null,
                    "fallback_policy": "none",
                    "timeout_ms": CUBE_LEASE_TIMEOUT.as_millis() as u64,
                    "fallback_from_prefer": prefer,
                })),
            )
            .await;

        CUBE_WORKSPACE_LEASE_ATTEMPTS.inc(&self.metrics);
        match self
            .invoke_lease(repo, task, None, CUBE_LEASE_TIMEOUT)
            .await
        {
            Ok(lease) => {
                CUBE_WORKSPACE_LEASE_SUCCESS.inc(&self.metrics);
                Ok(lease)
            }
            Err((reason, err)) => {
                tracing::error!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    worker_id,
                    cube_repo_id = %repo.repo_id,
                    reason,
                    error = format!("{err:#}"),
                    "cube workspace lease fallback also failed"
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(
                            Stage::CubeWorkspaceLeaseFailed,
                            DispatchOutcome::Error,
                            &execution.id,
                        )
                        .with_work_item(&execution.work_item_id)
                        .with_worker(worker_id)
                        .with_cube_repo(&repo.repo_id)
                        .with_error(&err)
                        .with_cube_invocation(attempt2_repr)
                        .with_details(serde_json::json!({
                            "attempt": 2,
                            "prefer_workspace_id": serde_json::Value::Null,
                            "reason": reason,
                            "fallback_policy": "none",
                            "fallback_from_prefer": prefer,
                        })),
                    )
                    .await;
                CUBE_WORKSPACE_LEASE_FAILURE.inc(&self.metrics);
                Err(err)
            }
        }
    }

    /// Run one `cube workspace lease` invocation under
    /// [`CUBE_LEASE_TIMEOUT`]. Returns `(reason, error)` so the caller
    /// can label the dispatch event with `"timeout"` vs `"cube_error"`
    /// without re-parsing the message.
    async fn invoke_lease(
        &self,
        repo: &CubeRepoHandle,
        task: &str,
        prefer_workspace_id: Option<&str>,
        timeout: Duration,
    ) -> std::result::Result<CubeWorkspaceLease, (&'static str, anyhow::Error)> {
        match tokio::time::timeout(
            timeout,
            self.host_adapter
                .lease_workspace(&repo.repo_id, task, prefer_workspace_id),
        )
        .await
        {
            Ok(Ok(lease)) => Ok(lease),
            Ok(Err(err)) => Err(("cube_error", err)),
            Err(_elapsed) => Err((
                "timeout",
                anyhow!("cube workspace lease timed out after {}s", timeout.as_secs()),
            )),
        }
    }

    /// Record a pre-start failure and either schedule an automatic retry
    /// or surface a permanent failure to the operator.
    ///
    /// Safe-to-retry stages (no worker side effects yet):
    /// `cube_repo_ensure`, `workspace_lease`, `change_create`,
    /// `run_start` (DB-only failure, transaction rolled back).
    ///
    /// Do NOT call this for post-`run_started` failures — those require
    /// `finish_execution_run`.
    fn record_start_failure(
        &self,
        coordinator: Arc<ExecutionCoordinator>,
        execution: &WorkExecution,
        worker_id: &str,
        cube_repo_id: Option<&str>,
        attention_kind: &str,
        attention_title: &str,
        error: &anyhow::Error,
    ) -> Result<()> {
        let (execution, run, outcome) = self.work_db.record_pre_start_failure(
            &execution.id,
            worker_id,
            cube_repo_id,
            &error.to_string(),
            &self.pre_start_retry_delays,
        )?;

        match outcome {
            PreStartFailureOutcome::Retry { delay } => {
                tracing::info!(
                    execution_id = %execution.id,
                    run_id = %run.id,
                    worker_id,
                    pre_start_failure_count = execution.pre_start_failure_count,
                    max_retries = self.pre_start_retry_delays.len(),
                    delay_secs = delay.as_secs(),
                    "pre-start failure will retry after backoff"
                );
                // After the backoff window expires, promote the execution
                // back into the ready queue and wake the scheduler. Until
                // then `dispatch_not_before` keeps it invisible to
                // `list_ready_executions`.
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    coordinator.kick();
                });
            }
            PreStartFailureOutcome::PermanentFail => {
                tracing::warn!(
                    execution_id = %execution.id,
                    run_id = %run.id,
                    worker_id,
                    pre_start_failure_count = execution.pre_start_failure_count,
                    error = %error,
                    "recorded execution start failure"
                );

                // Surface every permanent pre-start failure as a
                // `WorkAttentionItem` so the failure is diagnosable in one
                // bossctl call instead of needing a tracing-log tail.
                let attention_body = format!(
                    "Execution `{execution_id}` could not start on worker `{worker_id}` \
                     after {attempts} attempt(s).\n\n\
                     **Error:** {err}\n\n\
                     Inspect `dispatch-events/executions/{execution_id}/dispatch.jsonl` \
                     for the full stage timeline.",
                    execution_id = execution.id,
                    attempts = execution.pre_start_failure_count,
                    err = format!("{error:#}"),
                );
                if let Err(attention_err) =
                    self.work_db.create_attention_item(CreateAttentionItemInput {
                        execution_id: Some(execution.id.clone()),
                        work_item_id: None,
                        kind: attention_kind.to_owned(),
                        status: None,
                        title: attention_title.to_owned(),
                        body_markdown: attention_body,
                        resolved_at: None,
                    })
                {
                    tracing::error!(
                        ?attention_err,
                        execution_id = %execution.id,
                        "failed to record attention item for execution start failure",
                    );
                }

                let publisher = self.publisher.clone();
                let execution_id = execution.id.clone();
                let work_item_id = execution.work_item_id.clone();
                let status = execution.status.clone();
                let product_id = match self.work_db.get_work_item(&work_item_id) {
                    Ok(item) => Some(work_item_product_id(&item)),
                    Err(err) => {
                        tracing::warn!(
                            ?err,
                            %work_item_id,
                            "failed to resolve product for runtime broadcast"
                        );
                        None
                    }
                };
                tokio::spawn(async move {
                    publisher
                        .publish(
                            &execution_id,
                            &work_item_id,
                            &status,
                            "execution_start_failed",
                        )
                        .await;
                    if let Some(product_id) = product_id {
                        publisher
                            .publish_work_item_changed(
                                &product_id,
                                &work_item_id,
                                "execution_start_failed",
                            )
                            .await;
                    }
                });
            }
        }
        Ok(())
    }

    async fn run_execution(
        self: Arc<Self>,
        execution: WorkExecution,
        run: WorkRun,
        work_item: WorkItem,
        worker_id: String,
        lease: CubeWorkspaceLease,
        change: CubeChangeHandle,
    ) {
        // Keep the cube lease alive for the lifetime of the run. Without
        // this, the lease ages past `DEFAULT_LEASE_TTL_SECS` (30 min) in
        // the middle of any long-running worker, and the next
        // `cube workspace lease` call from another execution silently
        // reclaims the slot, runs `jj new <main>` against the workspace,
        // and moves the still-active worker's `@`. That's the
        // 2026-05-12 incident Worf reported on `mono-agent-001`.
        //
        // The heartbeat task is scoped to this function: it's aborted
        // on the JoinHandle drop at the end, so it can't outlive the
        // run and accidentally extend a lease the engine has already
        // released downstream.
        let heartbeat = HeartbeatGuard::spawn(
            Arc::clone(&self.host_adapter),
            lease.lease_id.clone(),
            execution.id.clone(),
            run.id.clone(),
            worker_id.clone(),
        );

        // Pre-spawn: collect the merge-tree diagnosis for revision_implementation
        // executions with merge-conflict provenance so compose_revision_directive
        // injects it into the worker prompt. No-op for other provenance.
        if execution.kind == "revision_implementation" {
            self.collect_revision_conflict_diagnosis_pre_spawn(&execution, &work_item, &lease)
                .await;
        }

        let run_outcome = self
            .host_adapter
            .spawn_worker(
                &worker_id,
                &execution,
                &work_item,
                lease.workspace_path.as_path(),
                Some(change.change_id.as_str()),
            )
            .await;
        drop(heartbeat);

        // Pane-spawn runs hand the slot to a live libghostty pane; the
        // WorkerPool slot must remain claimed until that pane is torn
        // down by `ServerState::release_worker_pane` (completion, force
        // release, or engine shutdown). Releasing it here would let a
        // concurrent dispatch re-claim the same slot while the pane
        // still owns it, and the app would reject `SpawnWorkerPane`
        // with `SlotBusy`. Non-pane runs (test fakes, future
        // ACP-style runners) leave `slot_id = None` and still need
        // the inline release.
        let defer_pool_slot_release = matches!(
            run_outcome.as_ref(),
            Ok(outcome) if outcome.slot_id.is_some()
        );

        match run_outcome {
            Ok(outcome) => {
                // Capture the resolved spawn knobs (effort level,
                // claude effort value, model) before `outcome` moves
                // into `record_run_completion` — they ride along on
                // the `pane_spawned` dispatch event below so a
                // diagnose verb can answer "what did the worker
                // actually launch with" without scraping process
                // argv. `None` from test fake runners that don't go
                // through `effort::resolve_spawn_config`.
                let spawn_config_for_event = outcome.spawn_config.clone();
                // If the runner allocated a real pane slot for this
                // run, stamp it onto the run record's agent_id so
                // `bossctl agents list` and related views show one
                // entry per active pane. Test runners that don't
                // allocate a pane leave slot_id as None and the
                // worker-pool placeholder (worker_id) stays as the
                // agent_id.
                let run = if let Some(slot_id) = outcome.slot_id {
                    let agent_id = format!("worker-{}", slot_id);
                    match self.work_db.set_run_agent_id(&run.id, &agent_id) {
                        Ok(updated) => updated,
                        Err(err) => {
                            tracing::error!(
                                ?err,
                                execution_id = %execution.id,
                                run_id = %run.id,
                                slot_id,
                                "failed to stamp pane slot onto run record"
                            );
                            run
                        }
                    }
                } else {
                    run
                };
                if let Err(err) = self
                    .record_run_completion(&execution, &run, &lease, &worker_id, outcome)
                    .await
                {
                    tracing::error!(
                        ?err,
                        execution_id = %execution.id,
                        run_id = %run.id,
                        worker_id = %worker_id,
                        "failed to record execution completion"
                    );
                }
                // Successful spawn → emit a structured `pane_spawned`
                // event so consumers can pair it with the
                // `cube_workspace_leased` event that preceded it and
                // see the full timeline. The `spawn_config` details
                // carry the effort + model tuple the dispatcher just
                // resolved — design §Q2 calls this out explicitly so
                // `bossctl dispatch diagnose <exec-id>` can answer
                // "which model / effort did this worker actually
                // launch with."
                let mut details = serde_json::json!({
                    "run_id": run.id,
                });
                if let Some(spawn) = spawn_config_for_event {
                    details["spawn_config"] = serde_json::json!({
                        "effort_level": spawn.effort_level.map(|level| level.as_str()),
                        "claude_effort": spawn.claude_effort,
                        "model": spawn.model,
                        "prompt_addendum_applied": spawn.prompt_addendum.is_some(),
                    });
                }
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::PaneSpawned, DispatchOutcome::Ok, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_worker(&worker_id)
                            .with_cube_lease(&lease.lease_id)
                            .with_cube_workspace(&lease.workspace_id)
                            .with_details(details),
                    )
                    .await;
            }
            Err(err) => {
                let released = match self.host_adapter.release_workspace(&lease.lease_id).await {
                    Ok(()) => true,
                    Err(release_err) => {
                        tracing::error!(
                            ?release_err,
                            execution_id = %execution.id,
                            run_id = %run.id,
                            lease_id = %lease.lease_id,
                            "failed to release workspace after run failure"
                        );
                        false
                    }
                };
                let error_text = err.to_string();

                // Historical silent-release path: a pane-spawn
                // failure (libghostty IPC drop, slot busy, prompt
                // composition error) inside `run_execution` marked
                // the run `failed` and released the lease without
                // raising anything the operator could see. Attach a
                // `WorkAttentionItem` to this run so the failure
                // turns up in the kanban "Attention" lane and via
                // `ListAttentionItems`. The structured event below
                // gives tooling a parallel signal.
                let attention = Some(CreateAttentionItemInput {
                    execution_id: Some(execution.id.clone()),
                    work_item_id: None,
                    kind: "pane_spawn_failed".to_owned(),
                    status: None,
                    title: "Worker pane failed to spawn".to_owned(),
                    body_markdown: format!(
                        "Execution `{exec_id}` leased workspace `{ws}` but the worker pane never came up.\n\n\
                         **Error:** {err}\n\n\
                         The lease was {release_state}. Inspect \
                         `dispatch-events/executions/{exec_id}/dispatch.jsonl` for the full stage timeline.",
                        exec_id = execution.id,
                        ws = lease.workspace_id,
                        err = format!("{err:#}"),
                        release_state = if released {
                            "released back to cube"
                        } else {
                            "still held by the engine (release failed — see the engine log)"
                        },
                    ),
                    resolved_at: None,
                });

                match self.work_db.finish_execution_run(
                    &execution.id,
                    &run.id,
                    "failed",
                    "failed",
                    None,
                    Some(error_text.as_str()),
                    released,
                    attention,
                ) {
                    Ok((execution, _run, _)) => {
                        tracing::warn!(
                            execution_id = %execution.id,
                            run_id = %run.id,
                            worker_id = %worker_id,
                            error = %err,
                            released_workspace = released,
                            "execution run failed"
                        );
                        self.dispatch_events
                            .emit(
                                DispatchEvent::new(
                                    Stage::PaneSpawned,
                                    DispatchOutcome::Error,
                                    &execution.id,
                                )
                                .with_work_item(&execution.work_item_id)
                                .with_worker(&worker_id)
                                .with_cube_lease(&lease.lease_id)
                                .with_cube_workspace(&lease.workspace_id)
                                .with_error(&err)
                                .with_details(serde_json::json!({
                                    "run_id": run.id,
                                    "released_workspace": released,
                                })),
                            )
                            .await;
                        // Clear the card out of `active`. The run is
                        // already recorded `failed` and the workspace
                        // released, but the work item itself stays
                        // `active` — so the kanban keeps the green
                        // "Doing" card and the orphan-active sweep
                        // re-dispatches the same doomed spawn every
                        // cycle. Demote it back to To-Do so the failure
                        // (already surfaced as a `pane_spawn_failed`
                        // attention item) is recoverable rather than a
                        // silent green-flicker strand.
                        match self
                            .work_db
                            .demote_active_work_item_to_todo(&execution.work_item_id)
                        {
                            Ok(true) => tracing::info!(
                                execution_id = %execution.id,
                                work_item_id = %execution.work_item_id,
                                "demoted work item to todo after pane-spawn failure",
                            ),
                            Ok(false) => {}
                            Err(demote_err) => tracing::error!(
                                ?demote_err,
                                work_item_id = %execution.work_item_id,
                                "failed to demote work item out of active after pane-spawn failure",
                            ),
                        }
                        self.publisher
                            .publish(
                                &execution.id,
                                &execution.work_item_id,
                                &execution.status,
                                "execution_run_failed",
                            )
                            .await;
                        if let Ok(item) = self.work_db.get_work_item(&execution.work_item_id) {
                            self.publisher
                                .publish_work_item_changed(
                                    &work_item_product_id(&item),
                                    &execution.work_item_id,
                                    "execution_run_failed",
                                )
                                .await;
                        }
                    }
                    Err(record_err) => {
                        tracing::error!(
                            ?record_err,
                            execution_id = %execution.id,
                            run_id = %run.id,
                            worker_id = %worker_id,
                            "failed to record execution run failure"
                        );
                    }
                }
            }
        }

        if !defer_pool_slot_release {
            self.release_worker_and_kick(&worker_id, Some(lease.workspace_id.as_str()))
                .await;
        }
    }

    /// Phase 3 cutover: for revision_implementation executions with merge-conflict
    /// provenance, resolve the linked `conflict_resolutions` row (via
    /// `created_via = "merge-conflict:<crz_id>"`) and collect its diagnosis:
    /// resolve the `conflict_resolutions` row a merge-conflict revision was
    /// spawned from (via `created_via = "merge-conflict:<crz_id>"`) and
    /// collect its diagnosis. No-op when the revision's provenance is not a
    /// merge conflict (e.g. operator/CI-fix revisions), or when a diagnosis
    /// is already stored (a respawn).
    async fn collect_revision_conflict_diagnosis_pre_spawn(
        &self,
        execution: &WorkExecution,
        work_item: &WorkItem,
        lease: &CubeWorkspaceLease,
    ) {
        let created_via = match work_item {
            WorkItem::Task(task) | WorkItem::Chore(task) => task.created_via.as_str(),
            _ => return,
        };
        let Some(crz_id) =
            created_via.strip_prefix(boss_protocol::CREATED_VIA_MERGE_CONFLICT_PREFIX)
        else {
            return;
        };
        let attempt = match self.work_db.get_conflict_resolution(crz_id) {
            Ok(Some(a)) => a,
            Ok(None) => {
                tracing::debug!(
                    execution_id = %execution.id,
                    crz_id,
                    "collect_conflict_diagnosis: revision's linked attempt row missing; skipping",
                );
                return;
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    crz_id,
                    ?err,
                    "collect_conflict_diagnosis: failed to look up revision's linked attempt; skipping",
                );
                return;
            }
        };
        if attempt.conflict_diagnosis.is_some() {
            tracing::debug!(
                attempt_id = %attempt.id,
                "collect_conflict_diagnosis: diagnosis already present on linked attempt; skipping",
            );
            return;
        }
        self.collect_conflict_diagnosis_for_attempt(&attempt, lease)
            .await;
    }

    /// Run `conflict_diagnosis::collect` in the leased workspace and persist
    /// the result on `attempt`. Shared by the bespoke `conflict_resolution`
    /// path and the Phase 3 merge-conflict revision path. Best-effort —
    /// failures are logged but never propagate.
    async fn collect_conflict_diagnosis_for_attempt(
        &self,
        attempt: &crate::work::ConflictResolution,
        lease: &CubeWorkspaceLease,
    ) {
        let base_sha = attempt.base_sha_at_trigger.as_deref().unwrap_or("");
        let head_sha = attempt.head_sha_before.as_deref().unwrap_or("");
        if base_sha.is_empty() || head_sha.is_empty() {
            tracing::debug!(
                attempt_id = %attempt.id,
                "collect_conflict_diagnosis: missing base/head sha; skipping",
            );
            return;
        }

        let diagnosis = match conflict_diagnosis::collect(
            &lease.workspace_path,
            base_sha,
            head_sha,
        )
        .await
        {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    workspace_path = %lease.workspace_path.display(),
                    ?err,
                    "collect_conflict_diagnosis: git spawn failed; using errored diagnosis",
                );
                conflict_diagnosis::ConflictDiagnosis::errored(
                    base_sha,
                    head_sha,
                    format!("git spawn failed: {err}"),
                )
            }
        };

        let json = match serde_json::to_string(&diagnosis) {
            Ok(j) => j,
            Err(err) => {
                tracing::warn!(attempt_id = %attempt.id, ?err, "collect_conflict_diagnosis: failed to serialize diagnosis");
                return;
            }
        };

        if let Err(err) = self
            .work_db
            .set_conflict_resolution_diagnosis(&attempt.id, &json)
        {
            tracing::warn!(
                attempt_id = %attempt.id,
                ?err,
                "collect_conflict_diagnosis: failed to persist diagnosis; continuing without it",
            );
        } else {
            tracing::debug!(
                attempt_id = %attempt.id,
                conflicted_files = diagnosis.files.len(),
                "collect_conflict_diagnosis: diagnosis persisted",
            );
        }
    }

    /// Release `worker_id` back to the pool, then rescan + kick to
    /// pick up newly-eligible work. Used at the tail of non-pane
    /// `run_execution` calls and from [`ServerState::release_worker_pane`]
    /// for the deferred pane-spawn case — the engine and the app must
    /// agree on which slots are busy, so the WorkerPool free signal is
    /// paired with the libghostty pane teardown rather than firing as
    /// soon as the spawn RPC returns.
    pub async fn release_worker_and_kick(
        self: &Arc<Self>,
        worker_id: &str,
        last_workspace_id: Option<&str>,
    ) {
        self.worker_pool
            .release_worker(worker_id, last_workspace_id)
            .await;
        self.rescan_active_dispatch_after_release();
        self.kick();
    }

    /// Steady-state rescan of `tasks.status = 'active'` work that
    /// never made it onto a worker. The create-time path already
    /// queues a `ready` execution and `kick()`s the scheduler, but a
    /// chore whose dispatch failed (cube lease error, kanban drag
    /// while the pool was full, worker died after starting) leaves
    /// the kanban card in `active` with a *terminal* (or absent)
    /// execution row — `list_ready_executions` skips it and `kick()`
    /// alone is not enough to reanimate it. Running
    /// [`WorkDb::rescan_active_dispatch`] before each kick fixes
    /// that: items whose latest execution is terminal (or missing)
    /// get a fresh `ready` row, and the scheduler picks them up on
    /// the just-released worker. Errors are logged and swallowed —
    /// the rescan is a best-effort opportunistic sweep, not a hard
    /// invariant.
    fn rescan_active_dispatch_after_release(&self) {
        match self.work_db.rescan_active_dispatch() {
            Ok(redispatched) if !redispatched.is_empty() => {
                tracing::info!(
                    count = redispatched.len(),
                    ids = ?redispatched,
                    "rescanned waiting active work after worker release",
                );
            }
            Ok(_) => {}
            Err(err) => {
                tracing::error!(
                    ?err,
                    "active-dispatch rescan failed after worker release; continuing",
                );
            }
        }
    }

    async fn record_run_completion(
        &self,
        execution: &WorkExecution,
        run: &WorkRun,
        lease: &CubeWorkspaceLease,
        worker_id: &str,
        outcome: RunOutcome,
    ) -> Result<()> {
        let release_workspace = outcome.wait_state.release_workspace();
        let released = if release_workspace {
            match self.host_adapter.release_workspace(&lease.lease_id).await {
                Ok(()) => true,
                Err(err) => {
                    tracing::error!(
                        ?err,
                        execution_id = %execution.id,
                        run_id = %run.id,
                        lease_id = %lease.lease_id,
                        "failed to release workspace after successful run"
                    );
                    false
                }
            }
        } else {
            false
        };

        let attention = outcome.attention.map(|attention| CreateAttentionItemInput {
            execution_id: Some(execution.id.clone()),
            work_item_id: None,
            kind: attention.kind,
            status: None,
            title: attention.title,
            body_markdown: attention.body_markdown,
            resolved_at: None,
        });

        let (execution, run, attention) = self.work_db.finish_execution_run(
            &execution.id,
            &run.id,
            outcome.wait_state.execution_status(),
            "completed",
            outcome.result_summary.as_deref(),
            None,
            released,
            attention,
        )?;

        tracing::info!(
            execution_id = %execution.id,
            run_id = %run.id,
            worker_id,
            execution_status = %execution.status,
            run_status = %run.status,
            attention_created = attention.is_some(),
            released_workspace = released,
            "execution run completed"
        );
        self.publisher
            .publish(
                &execution.id,
                &execution.work_item_id,
                &execution.status,
                "execution_run_completed",
            )
            .await;
        if let Ok(item) = self.work_db.get_work_item(&execution.work_item_id) {
            self.publisher
                .publish_work_item_changed(
                    &work_item_product_id(&item),
                    &execution.work_item_id,
                    "execution_run_completed",
                )
                .await;
        }
        Ok(())
    }
}

fn work_item_product_id(item: &WorkItem) -> String {
    match item {
        WorkItem::Product(p) => p.id.clone(),
        WorkItem::Project(p) => p.product_id.clone(),
        WorkItem::Task(t) | WorkItem::Chore(t) => t.product_id.clone(),
    }
}

/// One failing-check record after parsing `ci_remediations.failed_checks`
/// back from JSON. Mirrors `ci_watch::FailedCheckRecord` on the read side;
/// kept here as a separate owned type so the coordinator doesn't depend
/// on ci_watch's private serialization shape.
#[derive(Debug, Deserialize)]
struct FailedCheckJson {
    #[allow(dead_code)]
    name: String,
    conclusion: String,
    target_url: String,
    provider: String,
    #[serde(default)]
    provider_job_id: Option<String>,
}

/// Pick the worst-failing entry from a JSON-encoded `failed_checks`
/// list. Worst-first ordering per design §"pre-spawn fetch": FAILURE >
/// TIMED_OUT > CANCELLED > everything else. Returns `None` when the
/// JSON is empty / malformed / has no entry with an identifiable
/// provider job id at all.
fn pick_worst_failing_check(failed_checks_json: &str) -> Option<FailedCheckJson> {
    let parsed: Vec<FailedCheckJson> = serde_json::from_str(failed_checks_json).ok()?;
    if parsed.is_empty() {
        return None;
    }
    parsed.into_iter().min_by_key(|c| match c.conclusion.as_str() {
        "FAILURE" => 0,
        "TIMED_OUT" => 1,
        "CANCELLED" => 2,
        "STARTUP_FAILURE" => 3,
        _ => 4,
    })
}

/// Why `drain_ready_queue` returned. Re-entering the outer scheduler
/// loop immediately is fine for `QueueEmpty` (the post-drain wakeup
/// check decides whether to actually re-loop); `PoolExhausted` is
/// also fine because the post-`release_worker` `kick()` will spawn a
/// fresh scheduler anyway, and we only re-loop here when
/// `scheduling_pending` was raised after we started this drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainOutcome {
    /// No more `ready` rows in the database.
    QueueEmpty,
    /// Found a `ready` row but the worker pool had no idle slot;
    /// deferred to whoever releases a worker next.
    PoolExhausted,
}

fn execution_task_summary(execution: &WorkExecution, work_item: &WorkItem) -> String {
    match work_item {
        WorkItem::Product(product) => format!("{} {}", execution.kind, product.name),
        WorkItem::Project(project) => format!("{} {}", execution.kind, project.name),
        WorkItem::Task(task) | WorkItem::Chore(task) => format!("{} {}", execution.kind, task.name),
    }
}

fn execution_change_title(execution: &WorkExecution, work_item: &WorkItem) -> String {
    match work_item {
        WorkItem::Product(product) => format!("{}: {}", execution.kind, product.name),
        WorkItem::Project(project) => format!("{}: {}", execution.kind, project.name),
        WorkItem::Task(task) | WorkItem::Chore(task) => {
            format!("{}: {}", execution.kind, task.name)
        }
    }
}

/// Does `repo`'s cube pool config look like the auto-provisioned
/// defaults that `cube repo ensure` writes when a brand-new origin
/// turns up — i.e. nothing the operator has customised?
///
/// The check is conservative: every field has to look default. If any
/// of `main_branch`, `workspace_root`, `workspace_prefix`, or `source`
/// has been touched, we trust the operator and stay silent. The
/// advisory exists to nudge users who never noticed cube auto-cloned
/// into `~/.local/share/cube/workspaces`; once they run
/// `cube repo add` the next probe sees customised fields and the item
/// no longer surfaces.
fn repo_has_default_pool_config(repo: &CubeRepoSummary) -> bool {
    if repo.main_branch != "main" {
        return false;
    }
    if repo.source.is_some() {
        return false;
    }
    let expected_prefix = format!("{}-agent-", repo.repo_id);
    if repo.workspace_prefix != expected_prefix {
        return false;
    }
    workspace_root_is_cube_default(&repo.workspace_root)
}

/// Heuristic for "cube auto-provisioned this `workspace_root`". The
/// engine can't directly ask cube what its data dir is, so we compare
/// against cube's documented defaults: `$CUBE_DATA_DIR/workspaces`,
/// `$XDG_DATA_HOME/cube/workspaces`, or `~/.local/share/cube/workspaces`.
/// Anything else — including the `~/Documents/dev/workspaces` layout
/// the workspace rules recommend — is treated as customised.
fn workspace_root_is_cube_default(workspace_root: &Path) -> bool {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("CUBE_DATA_DIR") {
        candidates.push(PathBuf::from(path).join("workspaces"));
    }
    if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
        candidates.push(PathBuf::from(path).join("cube/workspaces"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".local/share/cube/workspaces"));
    }
    candidates.iter().any(|candidate| candidate == workspace_root)
}

/// Body for the `repo_cold_pool` advisory. Mirrors the design doc Q6
/// recommendation block so the user gets the exact `cube repo add`
/// override invocation, pre-filled with this repo's id and origin.
fn cold_repo_attention_body(repo: &CubeRepoSummary) -> String {
    format!(
        "First dispatch against `{repo_id}` ({origin}).\n\
         Cube auto-provisioned a pool at `{workspace_root}` with prefix `{prefix}`.\n\n\
         To customize, run:\n\n\
         ```\n\
         cube repo add {repo_id} \\\n    \
             --origin {origin} \\\n    \
             --workspace-root ~/Documents/dev/workspaces \\\n    \
             --workspace-prefix {repo_id}-agent\n\
         ```\n\n\
         Each pool has a configurable workspace count (concurrent workers per repo). \
         For multi-repo products this matters — see \
         `tools/boss/docs/designs/multi-repo-work-modeling.md` Q6. This item is \
         advisory; dispatch is proceeding with cube defaults.",
        repo_id = repo.repo_id,
        origin = repo.origin,
        workspace_root = repo.workspace_root.display(),
        prefix = repo.workspace_prefix,
    )
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use tempfile::tempdir;
    use tokio::sync::Mutex;
    use tokio::time::sleep;

    use super::{
        CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease,
        CubeWorkspaceStatus, ExecutionCoordinator, ExecutionPublisher, FrontendEvent,
        MAX_WORKER_POOL_SIZE, WorkerPool, pick_worst_failing_check, slot_id_from_worker_id,
    };

    #[test]
    fn pick_worst_failing_check_prefers_failure() {
        let json = serde_json::json!([
            {"name": "infra", "conclusion": "CANCELLED", "target_url": "https://buildkite.com/o/p/builds/2#j", "provider": "buildkite", "provider_job_id": "j"},
            {"name": "tests", "conclusion": "FAILURE", "target_url": "https://buildkite.com/o/p/builds/3#k", "provider": "buildkite", "provider_job_id": "k"},
            {"name": "x", "conclusion": "TIMED_OUT", "target_url": "https://buildkite.com/o/p/builds/4#l", "provider": "buildkite", "provider_job_id": "l"},
        ])
        .to_string();
        let picked = pick_worst_failing_check(&json).expect("expected one entry");
        assert_eq!(picked.conclusion, "FAILURE");
        assert_eq!(picked.provider, "buildkite");
        assert_eq!(picked.provider_job_id.as_deref(), Some("k"));
    }

    #[test]
    fn pick_worst_failing_check_handles_malformed_json() {
        assert!(pick_worst_failing_check("{not json}").is_none());
        assert!(pick_worst_failing_check("[]").is_none());
    }

    #[test]
    fn pick_worst_failing_check_falls_back_to_only_entry() {
        let json = serde_json::json!([
            {"name": "n", "conclusion": "STARTUP_FAILURE", "target_url": "u", "provider": "github_actions", "provider_job_id": "1"},
        ])
        .to_string();
        let picked = pick_worst_failing_check(&json).expect("entry");
        assert_eq!(picked.conclusion, "STARTUP_FAILURE");
    }
    use crate::runner::{ExecutionRunner, RunAttention, RunOutcome, RunWaitState};
    use crate::work::{
        CreateChoreInput, CreateProductInput, CreateProjectInput, CreateTaskInput,
        RequestExecutionInput, WorkDb, WorkExecution, WorkItem,
    };

    #[derive(Default)]
    struct FakeCubeClient {
        ensure_calls: Mutex<Vec<String>>,
        lease_calls: Mutex<Vec<(String, String, Option<String>)>>,
        create_calls: Mutex<Vec<(String, String)>>,
        release_calls: Mutex<Vec<String>>,
        status_calls: Mutex<Vec<PathBuf>>,
        heartbeat_calls: Mutex<Vec<(String, Option<u64>)>>,
        force_release_calls: Mutex<Vec<(String, Option<String>)>>,
        /// Counts how many times `list_repos` has been invoked. Tests
        /// for the cold-pool probe assert this equals 1 across two
        /// dispatches against the same URL (probe is engine-lifetime
        /// deduped).
        list_repos_calls: Mutex<u32>,
        /// Snapshot returned by `list_repos`. Default is the empty
        /// slice — most tests don't exercise the cold-pool probe and
        /// the empty list short-circuits before any attention item is
        /// written.
        repos: Mutex<Vec<CubeRepoSummary>>,
        fail_ensure: bool,
        fail_lease: bool,
        /// Simulate cube refusing a `--prefer` request because the
        /// preferred workspace is held: `lease_workspace` errors when
        /// `prefer_workspace_id` is `Some(_)`. Models the "prefer set,
        /// no fallback" path — the engine should fail fast rather than
        /// silently landing on a different workspace.
        fail_lease_when_prefer_set: bool,
        /// Fail the first N lease calls (0-indexed), then succeed. Used
        /// to model a single bad workspace being skipped via `any_free`
        /// retry when `preferred_workspace_id=null`.
        fail_first_n_leases: usize,
        fail_create: bool,
        next_workspace_id: Mutex<Option<String>>,
    }

    impl FakeCubeClient {
        fn with_next_workspace_id(self, id: impl Into<String>) -> Self {
            *self.next_workspace_id.try_lock().expect("uncontended") = Some(id.into());
            self
        }

        fn with_repos(self, repos: Vec<CubeRepoSummary>) -> Self {
            *self.repos.try_lock().expect("uncontended") = repos;
            self
        }
    }

    #[async_trait]
    impl CubeClient for FakeCubeClient {
        async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle> {
            self.ensure_calls.lock().await.push(origin.to_owned());
            if self.fail_ensure {
                return Err(anyhow!("cube repo ensure failed"));
            }
            Ok(CubeRepoHandle {
                repo_id: "mono".to_owned(),
            })
        }

        async fn lease_workspace(
            &self,
            repo_id: &str,
            task: &str,
            prefer_workspace_id: Option<&str>,
        ) -> Result<CubeWorkspaceLease> {
            let mut calls = self.lease_calls.lock().await;
            let call_index = calls.len();
            calls.push((
                repo_id.to_owned(),
                task.to_owned(),
                prefer_workspace_id.map(str::to_owned),
            ));
            drop(calls);
            if self.fail_lease {
                return Err(anyhow!("cube workspace lease failed"));
            }
            if self.fail_lease_when_prefer_set && prefer_workspace_id.is_some() {
                return Err(anyhow!(
                    "cube workspace lease failed: preferred workspace held by another worker"
                ));
            }
            if call_index < self.fail_first_n_leases {
                return Err(anyhow!(
                    "cube workspace lease failed: workspace has uncommitted work"
                ));
            }
            let workspace_id = self
                .next_workspace_id
                .lock()
                .await
                .clone()
                .or_else(|| prefer_workspace_id.map(str::to_owned))
                .unwrap_or_else(|| "mono-agent-001".to_owned());
            Ok(CubeWorkspaceLease {
                lease_id: "lease-1".to_owned(),
                workspace_id: workspace_id.clone(),
                workspace_path: PathBuf::from(format!("/tmp/{workspace_id}")),
            })
        }

        async fn create_change(
            &self,
            workspace_path: &PathBuf,
            title: &str,
        ) -> Result<CubeChangeHandle> {
            self.create_calls
                .lock()
                .await
                .push((workspace_path.display().to_string(), title.to_owned()));
            if self.fail_create {
                return Err(anyhow!("cube change create failed"));
            }
            Ok(CubeChangeHandle {
                change_id: "chg-1".to_owned(),
            })
        }

        async fn release_workspace(&self, lease_id: &str) -> Result<()> {
            self.release_calls.lock().await.push(lease_id.to_owned());
            Ok(())
        }

        async fn workspace_status(
            &self,
            workspace_path: &std::path::Path,
        ) -> Result<CubeWorkspaceStatus> {
            self.status_calls
                .lock()
                .await
                .push(workspace_path.to_path_buf());
            Ok(CubeWorkspaceStatus {
                workspace_id: "mono-agent-001".to_owned(),
                workspace_path: workspace_path.to_path_buf(),
                state: "leased".to_owned(),
                lease_id: Some("lease-1".to_owned()),
                holder: Some("boss/0".to_owned()),
                task: Some("test task".to_owned()),
                leased_at_epoch_s: Some(1_700_000_000),
                lease_expires_at_epoch_s: Some(1_700_001_800),
            })
        }

        async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()> {
            self.heartbeat_calls
                .lock()
                .await
                .push((lease_id.to_owned(), ttl_seconds));
            Ok(())
        }

        async fn force_release_lease(
            &self,
            lease_id: &str,
            reason: Option<&str>,
        ) -> Result<()> {
            self.force_release_calls
                .lock()
                .await
                .push((lease_id.to_owned(), reason.map(str::to_owned)));
            Ok(())
        }

        async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
            Ok(Vec::new())
        }

        async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
            *self.list_repos_calls.lock().await += 1;
            Ok(self.repos.lock().await.clone())
        }
    }

    struct FakeExecutionRunner {
        calls: Mutex<Vec<(String, String, String, Option<String>)>>,
        fail: bool,
        pending: bool,
        /// If `Some`, the runner reports this slot id back to the
        /// coordinator in the `RunOutcome`, simulating a successful
        /// `SpawnWorkerPane` round-trip. Used to verify that the
        /// coordinator stamps the slot-based agent_id onto the run
        /// record.
        slot_id: Option<u8>,
        /// Resolved spawn knobs the fake runner reports back. `None`
        /// matches the default fake-runner contract (no effort/model
        /// resolution happened). Production `PaneSpawnRunner` always
        /// fills this in — tests that want to assert on the
        /// dispatcher's effort/model surfacing set it explicitly.
        spawn_config: Option<crate::effort::SpawnConfig>,
    }

    impl Default for FakeExecutionRunner {
        fn default() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail: false,
                pending: false,
                slot_id: None,
                spawn_config: None,
            }
        }
    }

    #[async_trait]
    impl ExecutionRunner for FakeExecutionRunner {
        async fn run_execution(
            &self,
            worker_id: &str,
            execution: &WorkExecution,
            work_item: &WorkItem,
            workspace_path: &std::path::Path,
            cube_change_id: Option<&str>,
        ) -> Result<RunOutcome> {
            self.calls.lock().await.push((
                worker_id.to_owned(),
                execution.id.clone(),
                workspace_path.display().to_string(),
                cube_change_id.map(str::to_owned),
            ));
            if self.pending {
                pending::<()>().await;
            }
            if self.fail {
                return Err(anyhow!("worker prompt failed"));
            }

            Ok(RunOutcome {
                wait_state: RunWaitState::WaitingHuman,
                result_summary: Some(format!("finished {}", execution.kind)),
                attention: Some(RunAttention {
                    kind: "review_required".to_owned(),
                    title: format!("Review {}", execution.kind),
                    body_markdown: format!("Review {}", test_work_item_name(work_item)),
                }),
                slot_id: self.slot_id,
                spawn_config: self.spawn_config.clone(),
            })
        }
    }

    #[derive(Default)]
    struct RecordingPublisher {
        events: Mutex<Vec<(String, String, String, String)>>,
        work_item_events: Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait]
    impl ExecutionPublisher for RecordingPublisher {
        async fn publish(
            &self,
            execution_id: &str,
            work_item_id: &str,
            status: &str,
            reason: &str,
        ) {
            self.events.lock().await.push((
                execution_id.to_owned(),
                work_item_id.to_owned(),
                status.to_owned(),
                reason.to_owned(),
            ));
        }

        async fn publish_work_item_changed(
            &self,
            product_id: &str,
            work_item_id: &str,
            reason: &str,
        ) {
            self.work_item_events.lock().await.push((
                product_id.to_owned(),
                work_item_id.to_owned(),
                reason.to_owned(),
            ));
        }

        async fn publish_frontend_event_on_product(
            &self,
            _product_id: &str,
            _event: FrontendEvent,
        ) {
        }
    }

    async fn wait_for_execution_status(db: &WorkDb, execution_id: &str, expected: &str) {
        for _ in 0..100 {
            let execution = db.get_execution(execution_id).unwrap();
            if execution.status == expected {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("execution {execution_id} never reached status `{expected}`");
    }

    fn test_work_item_name(work_item: &WorkItem) -> &str {
        match work_item {
            WorkItem::Product(product) => &product.name,
            WorkItem::Project(project) => &project.name,
            WorkItem::Task(task) | WorkItem::Chore(task) => &task.name,
        }
    }

    #[tokio::test]
    async fn schedules_ready_execution_into_running_run() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            runner.clone(),
        ));
        coordinator.kick();
        wait_for_execution_status(
            db.as_ref(),
            &db.list_executions(Some(&chore.id)).unwrap()[0].id,
            "running",
        )
        .await;

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        assert_eq!(execution.status, "running");
        assert_eq!(execution.cube_repo_id.as_deref(), Some("mono"));
        assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
        let run = db.list_runs(&execution.id).unwrap().pop().unwrap();
        assert_eq!(run.agent_id, "worker-1");
        assert_eq!(run.status, "active");
        assert_eq!(coordinator.worker_pool().idle_count().await, 0);
        assert_eq!(cube.ensure_calls.lock().await.len(), 1);
        assert_eq!(cube.lease_calls.lock().await.len(), 1);
        assert_eq!(cube.create_calls.lock().await.len(), 1);
        assert_eq!(runner.calls.lock().await.len(), 1);
        assert_eq!(runner.calls.lock().await[0].3.as_deref(), Some("chg-1"));
    }

    /// `cube_default_workspace_root_for_test` mirrors the production
    /// helper so tests can construct a `workspace_root` value that
    /// `workspace_root_is_cube_default` would accept, without
    /// mutating process-wide env vars (which would race other tests
    /// in the same crate).
    fn cube_default_workspace_root_for_test() -> PathBuf {
        if let Some(d) = std::env::var_os("CUBE_DATA_DIR") {
            return PathBuf::from(d).join("workspaces");
        }
        if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(d).join("cube/workspaces");
        }
        let home = std::env::var_os("HOME").expect(
            "test requires HOME, CUBE_DATA_DIR, or XDG_DATA_HOME to be set so we can \
             construct a cube-default workspace_root that the helper recognises",
        );
        PathBuf::from(home).join(".local/share/cube/workspaces")
    }

    /// Q6 / Follow-up chore #8: the cold-repo probe raises an
    /// advisory `repo_cold_pool` attention item on the first dispatch
    /// against a previously-unseen URL whose cube pool config matches
    /// auto-provision defaults. Across two dispatches against the
    /// same URL only one item is written, and `cube repo list` is
    /// only called once — both dispatches still drive the execution
    /// to `running`.
    #[tokio::test]
    async fn cold_repo_probe_raises_advisory_once_across_repeated_dispatches() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let origin = "git@github.com:spinyfin/mono.git";
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some(origin.to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        // Two chores → two executions against the same product/URL.
        let chore_a = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup A".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let chore_b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        // Cube reports a single repo whose pool config exactly
        // matches the auto-provisioned defaults — `cube repo add`
        // / `cube repo configure` were never run.
        let default_repo = CubeRepoSummary {
            repo_id: "mono".to_owned(),
            origin: origin.to_owned(),
            main_branch: "main".to_owned(),
            workspace_root: cube_default_workspace_root_for_test(),
            workspace_prefix: "mono-agent-".to_owned(),
            source: None,
        };
        let cube = Arc::new(
            FakeCubeClient::default().with_repos(vec![default_repo]),
        );
        // Pool size 2 so both executions can dispatch concurrently.
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(2),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        ));
        coordinator.kick();

        let exec_a = db.list_executions(Some(&chore_a.id)).unwrap().pop().unwrap();
        let exec_b = db.list_executions(Some(&chore_b.id)).unwrap().pop().unwrap();
        wait_for_execution_status(db.as_ref(), &exec_a.id, "running").await;
        wait_for_execution_status(db.as_ref(), &exec_b.id, "running").await;

        // Two ensure_repo calls (one per execution), but list_repos
        // was deduplicated to exactly one round-trip.
        assert_eq!(cube.ensure_calls.lock().await.len(), 2);
        assert_eq!(*cube.list_repos_calls.lock().await, 1);

        // Exactly one advisory item across both executions. It
        // attaches to the execution that hit the probe first.
        let attn_a = db.list_attention_items(&exec_a.id).unwrap();
        let attn_b = db.list_attention_items(&exec_b.id).unwrap();
        let cold_items: Vec<_> = attn_a
            .iter()
            .chain(attn_b.iter())
            .filter(|item| item.kind == "repo_cold_pool")
            .collect();
        assert_eq!(
            cold_items.len(),
            1,
            "expected exactly one repo_cold_pool item across both executions, \
             got {} (exec_a: {} items, exec_b: {} items)",
            cold_items.len(),
            attn_a.len(),
            attn_b.len(),
        );
        let item = cold_items[0];
        assert_eq!(item.status, "open");
        assert!(
            item.body_markdown.contains("cube repo add mono"),
            "body should name the override command verbatim; got: {}",
            item.body_markdown,
        );
        assert!(
            item.body_markdown.contains(origin),
            "body should echo the repo origin; got: {}",
            item.body_markdown,
        );
    }

    /// A repo whose cube pool config has been customised (custom
    /// `workspace_root` or `workspace_prefix`) is the steady-state we
    /// don't want to nag about. Even though it's the first dispatch
    /// in this engine's lifetime, no `repo_cold_pool` item should
    /// land.
    #[tokio::test]
    async fn cold_repo_probe_stays_silent_when_pool_is_customised() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let origin = "git@github.com:spinyfin/mono.git";
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some(origin.to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let custom_repo = CubeRepoSummary {
            repo_id: "mono".to_owned(),
            origin: origin.to_owned(),
            main_branch: "main".to_owned(),
            workspace_root: PathBuf::from("/Users/operator/Documents/dev/workspaces"),
            workspace_prefix: "mono-agent-".to_owned(),
            source: None,
        };
        let cube = Arc::new(
            FakeCubeClient::default().with_repos(vec![custom_repo]),
        );
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        ));
        coordinator.kick();
        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        wait_for_execution_status(db.as_ref(), &execution.id, "running").await;

        assert_eq!(*cube.list_repos_calls.lock().await, 1);
        let items = db.list_attention_items(&execution.id).unwrap();
        assert!(
            items.iter().all(|i| i.kind != "repo_cold_pool"),
            "no repo_cold_pool item should be raised for a customised pool; got: {:?}",
            items.iter().map(|i| &i.kind).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn repo_has_default_pool_config_recognises_defaults_only() {
        use super::{repo_has_default_pool_config, CubeRepoSummary};
        // A repo whose every field matches the auto-provisioned
        // defaults — the case the probe should flag.
        let default_root = cube_default_workspace_root_for_test();
        let base = CubeRepoSummary {
            repo_id: "nimbus".to_owned(),
            origin: "git@github.com:myorg/nimbus.git".to_owned(),
            main_branch: "main".to_owned(),
            workspace_root: default_root.clone(),
            workspace_prefix: "nimbus-agent-".to_owned(),
            source: None,
        };
        assert!(repo_has_default_pool_config(&base));

        // A custom main_branch means the operator has touched the
        // config — stay silent.
        let mut customised = base.clone();
        customised.main_branch = "trunk".to_owned();
        assert!(!repo_has_default_pool_config(&customised));

        // `source` overlay means the user is sharing a local clone;
        // pool is explicitly configured.
        let mut with_source = base.clone();
        with_source.source = Some(PathBuf::from("/Users/dev/Documents/dev/nimbus"));
        assert!(!repo_has_default_pool_config(&with_source));

        // Custom workspace_prefix that doesn't match the auto-derived
        // `{repo_id}-agent-` shape.
        let mut custom_prefix = base.clone();
        custom_prefix.workspace_prefix = "nimbus-pool-".to_owned();
        assert!(!repo_has_default_pool_config(&custom_prefix));

        // Custom workspace_root anywhere outside cube's data dir.
        let mut custom_root = base;
        custom_root.workspace_root = PathBuf::from("/Users/dev/Documents/dev/workspaces");
        assert!(!repo_has_default_pool_config(&custom_root));
    }

    #[tokio::test]
    async fn slot_id_from_outcome_is_stamped_onto_run_agent_id() {
        // When the runner reports a real pane slot back via
        // RunOutcome.slot_id, the coordinator must overwrite the run
        // record's `agent_id` with `worker-{slot}` before recording
        // completion. This is what makes `bossctl agents list` show
        // one entry per active pane instead of collapsing every
        // dispatched run into the worker-pool placeholder.
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        // Pool has only one slot, so the worker-pool placeholder
        // would otherwise be `worker-1`. The runner reports slot 5
        // — the assertion below proves the slot value won, not the
        // pool placeholder.
        let runner = Arc::new(FakeExecutionRunner {
            slot_id: Some(5),
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            runner,
        ));
        coordinator.kick();

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        wait_for_execution_status(db.as_ref(), &execution.id, "waiting_human").await;

        let run = db.list_runs(&execution.id).unwrap().pop().unwrap();
        assert_eq!(run.status, "completed");
        assert_eq!(run.agent_id, "worker-5");
    }

    #[tokio::test]
    async fn pane_spawn_run_does_not_release_worker_pool_slot() {
        // The libghostty pane outlives the `run_execution` call —
        // PaneSpawnRunner returns Ok(WaitingHuman) the instant the
        // SpawnWorkerPane RPC completes, but the user-visible worker
        // is just getting started. If the coordinator freed the
        // WorkerPool slot at that moment, the next dispatch could
        // re-claim the slot and the app would reject the spawn with
        // SlotBusy. Outcomes that carry slot_id = Some(N) must keep
        // the slot claimed until `release_worker_pane` fires.
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            slot_id: Some(1),
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            runner,
        ));
        coordinator.kick();

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        wait_for_execution_status(db.as_ref(), &execution.id, "waiting_human").await;

        // Slot 1 still belongs to the (notionally) live pane. Only
        // `release_worker_pane` (driven by completion / force release
        // / shutdown) is allowed to free it.
        assert_eq!(
            coordinator.worker_pool().idle_count().await,
            0,
            "WorkerPool slot must stay claimed while the libghostty pane is alive"
        );
    }

    #[tokio::test]
    async fn release_worker_and_kick_frees_pool_slot() {
        // The deferred-release helper called from
        // `ServerState::release_worker_pane` after the pane RPC
        // returns. After it runs, the matching pool slot is idle
        // again and the next claim succeeds.
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db,
            WorkerPool::new(2),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        ));

        let claimed = coordinator
            .worker_pool()
            .claim_worker("exec-pre", None)
            .await
            .expect("pool has free slots");
        assert_eq!(coordinator.worker_pool().idle_count().await, 1);

        coordinator
            .release_worker_and_kick(&claimed, Some("ws-1"))
            .await;

        assert_eq!(
            coordinator.worker_pool().idle_count().await,
            2,
            "release_worker_and_kick must return the slot to the idle pool",
        );
        // Idempotent: a second release on the same already-idle slot
        // is a no-op (the pane-spawn lifecycle can racily re-enter
        // this path from completion + chore-done).
        coordinator
            .release_worker_and_kick(&claimed, Some("ws-1"))
            .await;
        assert_eq!(coordinator.worker_pool().idle_count().await, 2);
    }

    #[tokio::test]
    async fn missing_slot_id_leaves_worker_pool_placeholder_in_agent_id() {
        // Runners without a pane leave slot_id = None. The coordinator
        // must not touch agent_id in that case — the worker-pool
        // placeholder set at run-create time stays.
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            runner,
        ));
        coordinator.kick();

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        wait_for_execution_status(db.as_ref(), &execution.id, "waiting_human").await;

        let run = db.list_runs(&execution.id).unwrap().pop().unwrap();
        assert_eq!(run.agent_id, "worker-1");
    }

    #[tokio::test]
    async fn successful_run_moves_execution_to_waiting_human_and_releases_worker() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            runner,
        ));
        coordinator.kick();

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        wait_for_execution_status(db.as_ref(), &execution.id, "waiting_human").await;

        let execution = db.get_execution(&execution.id).unwrap();
        assert_eq!(execution.status, "waiting_human");
        assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
        let run = db.list_runs(&execution.id).unwrap().pop().unwrap();
        assert_eq!(run.status, "completed");
        assert_eq!(coordinator.worker_pool().idle_count().await, 1);
        assert_eq!(db.list_attention_items(&execution.id).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn start_failure_marks_execution_failed_and_releases_worker() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_lease: true,
            ..FakeCubeClient::default()
        });
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(vec![]),
        );
        coordinator.kick();
        wait_for_execution_status(
            db.as_ref(),
            &db.list_executions(Some(&chore.id)).unwrap()[0].id,
            "failed",
        )
        .await;

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        assert_eq!(execution.status, "failed");
        let run = db.list_runs(&execution.id).unwrap().pop().unwrap();
        assert_eq!(run.status, "failed");
        assert_eq!(
            run.error_text.as_deref(),
            Some("cube workspace lease failed")
        );
        assert_eq!(coordinator.worker_pool().idle_count().await, 1);
    }

    /// Operators previously saw lease failures show up as a vague
    /// "no slot available" because the engine swallowed the cube
    /// stderr. The dispatcher now logs the full anyhow chain at
    /// `tracing::error!` *before* `record_start_failure` writes its
    /// own warn line, so the verbatim cube stderr lands in the
    /// engine log. Stale-working-copy recovery is owned by cube
    /// (cube PR #254); this test only pins the loud-logging
    /// contract.
    #[tokio::test]
    async fn lease_failure_logs_cube_stderr_at_error_before_recording_failure() {
        let buffer = log_capture::install();
        let starting_offset = buffer.lock().len();

        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_lease: true,
            ..FakeCubeClient::default()
        });
        // No retries: go straight to permanent failure so the test does
        // not have to wait through exponential backoff delays.
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(vec![]),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, "failed").await;

        // Slice out only the bytes written after the test started so
        // we don't trip over events emitted by other parallel tests
        // sharing the same global subscriber.
        let captured =
            String::from_utf8_lossy(&buffer.lock()[starting_offset..]).to_string();
        let our_lines: Vec<&str> = captured
            .lines()
            .filter(|line| line.contains(&execution_id))
            .collect();
        assert!(
            !our_lines.is_empty(),
            "expected captured log lines for execution {execution_id}, got nothing.\n\
             Full slice was:\n{captured}"
        );

        let error_idx = our_lines
            .iter()
            .position(|line| {
                line.contains("ERROR")
                    && line.contains("cube workspace lease attempt failed")
            })
            .unwrap_or_else(|| {
                panic!(
                    "expected a tracing::error! log for the cube lease failure;\n\
                     captured lines for this execution were:\n{:#?}",
                    our_lines
                )
            });
        let error_line = our_lines[error_idx];
        // The fake's lease error message *is* the simulated cube
        // stderr; the engine must surface it verbatim rather than
        // truncating or pattern-matching.
        assert!(
            error_line.contains("cube workspace lease failed"),
            "error log line must include the cube stderr verbatim, got:\n{error_line}"
        );

        let warn_idx = our_lines
            .iter()
            .position(|line| {
                line.contains("WARN") && line.contains("recorded execution start failure")
            })
            .unwrap_or_else(|| {
                panic!(
                    "expected a tracing::warn! log from record_start_failure;\n\
                     captured lines for this execution were:\n{:#?}",
                    our_lines
                )
            });

        assert!(
            error_idx < warn_idx,
            "error log must precede record_start_failure's warn log; \
             got error at {error_idx}, warn at {warn_idx}.\n\
             Captured lines:\n{:#?}",
            our_lines
        );
    }

    /// Shared per-process tracing capture used by tests that need
    /// to assert on log output. We can't install a per-test
    /// subscriber because cargo runs library tests in parallel
    /// threads of the same process and `set_global_default`
    /// rejects a second installer. Tests that opt in slice the
    /// shared buffer by execution_id (which is unique per test) to
    /// isolate their own events.
    mod log_capture {
        use std::io;
        use std::sync::{Arc, Mutex, OnceLock};

        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        pub(super) struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

        impl SharedBuffer {
            pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
                self.0.lock().expect("shared log buffer poisoned")
            }
        }

        struct SharedWriter(Arc<Mutex<Vec<u8>>>);

        impl io::Write for SharedWriter {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                self.0
                    .lock()
                    .expect("shared log buffer poisoned")
                    .extend_from_slice(data);
                Ok(data.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        struct SharedMakeWriter(Arc<Mutex<Vec<u8>>>);

        impl<'a> MakeWriter<'a> for SharedMakeWriter {
            type Writer = SharedWriter;

            fn make_writer(&'a self) -> Self::Writer {
                SharedWriter(self.0.clone())
            }
        }

        pub(super) fn install() -> SharedBuffer {
            static BUFFER: OnceLock<SharedBuffer> = OnceLock::new();
            BUFFER
                .get_or_init(|| {
                    let buffer = SharedBuffer(Arc::new(Mutex::new(Vec::new())));
                    let subscriber = tracing_subscriber::fmt()
                        .with_writer(SharedMakeWriter(buffer.0.clone()))
                        .with_ansi(false)
                        .with_target(false)
                        .with_max_level(tracing::Level::TRACE)
                        .finish();
                    // Tolerate the "already set" race: another test
                    // binary or a stray init in the same process
                    // shouldn't sink the suite. The capture only
                    // works if our subscriber wins, but if it
                    // doesn't, the assertions below will fail
                    // loudly with a clear "no captured lines"
                    // message.
                    let _ = tracing::subscriber::set_global_default(subscriber);
                    buffer
                })
                .clone()
        }
    }

    /// Regression for the silent-release dispatch failure: when the
    /// pane-spawn step inside `run_execution` fails — libghostty IPC
    /// drop, prompt composition error, runner panic, all surface
    /// here as `Err(_)` from `ExecutionRunner::run_execution` — the
    /// coordinator MUST raise a `WorkAttentionItem` AND emit a
    /// structured `pane_spawned` error event. Before this fix
    /// landed, the run flipped to `failed` and the lease was
    /// released, but nothing surfaced to `bossctl agents list` or
    /// the kanban view; operators had nothing to chase. The
    /// `RecordingDispatchEventSink` below asserts the stage timeline
    /// reaches `pane_spawned: error`; the `list_attention_items`
    /// assertion proves the WorkAttentionItem made it to disk.
    #[tokio::test]
    async fn pane_spawn_failure_raises_attention_item_and_dispatch_event() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            fail: true,
            ..FakeExecutionRunner::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
                .with_dispatch_events(recording.clone()),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, "failed").await;

        // The execution went all the way through the lease + change
        // creation. `rescan_active_dispatch_after_release` will
        // re-queue the chore (pre-existing retry behavior, since
        // `start_execution_run` flipped tasks.status to `active`
        // before the spawn failed), so cube fakes may be invoked
        // multiple times — pin only "at least once each".
        assert!(!cube.lease_calls.lock().await.is_empty());
        assert!(!cube.create_calls.lock().await.is_empty());
        // The lease is released after the pane-spawn failure — before
        // the fix, this release was the *only* observable signal that
        // anything went wrong.
        assert!(
            cube.release_calls
                .lock()
                .await
                .iter()
                .any(|id| id == "lease-1")
        );

        // Loud signal #1: the WorkAttentionItem is what surfaces in
        // the kanban "Attention" lane and through `ListAttentionItems`.
        // The exact count varies — once the run finishes_execution_run
        // with `failed`, `rescan_active_dispatch_after_release` will
        // see the chore is still in `active` status (auto-advanced
        // when `start_execution_run` committed) and re-queue another
        // ready execution, which fails again. That retry behavior is
        // pre-existing; this test only pins the loud-failure contract:
        // every failed pane spawn raises exactly one attention item.
        let attention_items = db.list_attention_items(&execution_id).unwrap();
        assert!(
            !attention_items.is_empty(),
            "pane-spawn failure must raise at least one attention item; got nothing",
        );
        let first = &attention_items[0];
        assert_eq!(first.kind, "pane_spawn_failed");
        assert!(
            first.body_markdown.contains("worker pane never came up"),
            "attention body should describe the failure mode; got {:?}",
            first.body_markdown,
        );
        assert!(
            first.body_markdown.contains("worker prompt failed"),
            "attention body should include the original error; got {:?}",
            first.body_markdown,
        );

        // Loud signal #2: a structured `pane_spawned: error` event in
        // the dispatch stream, so external tooling can flag it
        // without scanning tracing logs.
        let events = recording.events_for(&execution_id).await;
        let pane_event = events
            .iter()
            .find(|event| event.stage == "pane_spawned" && event.outcome == "error")
            .unwrap_or_else(|| {
                panic!("expected a pane_spawned:error event for {execution_id}; got {events:#?}")
            });
        assert!(
            pane_event
                .error_message
                .as_deref()
                .is_some_and(|msg| msg.contains("worker prompt failed")),
            "pane_spawned event must include the underlying error; got {:?}",
            pane_event.error_message,
        );
        // The stage timeline before the failure should also be
        // visible — request_recorded, worker_claimed, cube stages,
        // run_started — so an operator can confirm dispatch did get
        // through every earlier handoff. `cube_workspace_lease_attempted`
        // sits between `cube_repo_ensured` and `cube_workspace_leased`
        // and pins what the engine asked cube to do (preferred
        // workspace, fallback policy) for diagnose visibility.
        let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();
        for expected in [
            "request_recorded",
            "worker_claimed",
            "cube_repo_ensured",
            "cube_workspace_lease_attempted",
            "cube_workspace_leased",
            "cube_change_created",
            "run_started",
            "pane_spawned",
        ] {
            assert!(
                stages.contains(&expected),
                "stage `{expected}` missing from dispatch timeline; got {stages:?}",
            );
        }
    }

    /// The `pane_spawned: ok` event must carry the resolved spawn
    /// knobs (effort level, claude effort value, model) so
    /// `bossctl dispatch diagnose <exec-id>` can answer "what did
    /// this worker actually launch with" — design §Q2 ("surfaces the
    /// chosen model, effort value, and level on the dispatch
    /// instrumentation stream"). The fake runner reports a synthetic
    /// `SpawnConfig`; this test pins that the coordinator forwards
    /// it into the event's `details.spawn_config` field.
    #[tokio::test]
    async fn pane_spawned_event_carries_spawn_config_details() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Trivial chore".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: Some(crate::work::EffortLevel::Trivial),
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            slot_id: Some(1),
            spawn_config: Some(crate::effort::SpawnConfig {
                effort_level: Some(crate::work::EffortLevel::Trivial),
                claude_effort: Some("low"),
                model: "haiku".to_owned(),
                prompt_addendum: None,
            }),
            ..FakeExecutionRunner::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        let coordinator = Arc::new(
            ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube, runner)
                .with_dispatch_events(recording.clone()),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, "waiting_human").await;

        let events = recording.events_for(&execution_id).await;
        let pane_event = events
            .iter()
            .find(|event| event.stage == "pane_spawned" && event.outcome == "ok")
            .unwrap_or_else(|| {
                panic!("expected pane_spawned:ok event for {execution_id}; got {events:#?}")
            });
        let spawn = pane_event
            .details
            .get("spawn_config")
            .unwrap_or_else(|| {
                panic!(
                    "pane_spawned event missing spawn_config in details: {:?}",
                    pane_event.details
                )
            });
        assert_eq!(spawn["effort_level"], "trivial");
        assert_eq!(spawn["claude_effort"], "low");
        assert_eq!(spawn["model"], "haiku");
        assert_eq!(spawn["prompt_addendum_applied"], false);
    }

    /// Cube lease failures also need the loud-failure contract: a
    /// `WorkAttentionItem` AND a structured event. This pins both —
    /// the older `lease_failure_logs_cube_stderr_at_error_before_recording_failure`
    /// test only asserts the tracing log shape.
    #[tokio::test]
    async fn cube_lease_failure_raises_attention_item_and_dispatch_event() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_lease: true,
            ..FakeCubeClient::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        // No retries: go straight to permanent failure so the test does
        // not have to wait through exponential backoff delays.
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(vec![])
            .with_dispatch_events(recording.clone()),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, "failed").await;

        let attention_items = db.list_attention_items(&execution_id).unwrap();
        assert_eq!(
            attention_items.len(),
            1,
            "cube lease failure must raise exactly one attention item",
        );
        assert_eq!(attention_items[0].kind, "cube_workspace_lease_failed");
        assert!(attention_items[0]
            .body_markdown
            .contains("cube workspace lease failed"));

        let events = recording.events_for(&execution_id).await;
        // The lease attempt event is emitted before the call, so the
        // timeline pins what the engine *intended* to do even when
        // cube refuses.
        let attempt_event = events
            .iter()
            .find(|event| event.stage == "cube_workspace_lease_attempted")
            .expect("cube_workspace_lease_attempted event missing");
        assert_eq!(attempt_event.outcome, "ok");
        assert_eq!(
            attempt_event.details.get("attempt").and_then(|v| v.as_u64()),
            Some(1),
            "first attempt event should carry attempt=1; got {:?}",
            attempt_event.details,
        );

        let lease_failed = events
            .iter()
            .find(|event| event.stage == "cube_workspace_lease_failed")
            .expect("cube_workspace_lease_failed event missing");
        assert_eq!(lease_failed.outcome, "error");
        assert!(
            lease_failed
                .error_message
                .as_deref()
                .is_some_and(|m| m.contains("cube workspace lease failed")),
            "lease_failed event must carry the verbatim cube error; got {:?}",
            lease_failed.error_message,
        );
        assert_eq!(
            lease_failed.details.get("reason").and_then(|v| v.as_str()),
            Some("cube_error"),
            "lease_failed event must classify reason; got {:?}",
            lease_failed.details,
        );

        // The success event must NOT be emitted, and the timeline
        // must NOT include later stages — dispatch bailed at the
        // lease step.
        let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();
        assert!(
            !stages.contains(&"cube_workspace_leased"),
            "cube_workspace_leased (success) must not appear when lease fails; got {stages:?}",
        );
        assert!(!stages.contains(&"cube_change_created"));
        assert!(!stages.contains(&"run_started"));
        assert!(!stages.contains(&"pane_spawned"));
    }

    /// Pre-start failures (cube lease error, cube ensure error, etc.) should
    /// be retried automatically before surfacing to the operator.
    ///
    /// This test uses zero-length backoff delays and a single retry slot so
    /// it runs quickly. It verifies:
    /// 1. A single pre-start failure resets the execution to `ready` (not
    ///    `failed`) and `pre_start_failure_count` is incremented.
    /// 2. A second failure (after retry) permanently marks the execution
    ///    `failed` and surfaces an attention item.
    /// 3. Only one execution row exists (no sibling rows).
    #[tokio::test]
    async fn pre_start_failure_retries_then_permanently_fails() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Retry Chore".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_lease: true,
            ..FakeCubeClient::default()
        });
        // One retry (two attempts total), immediate backoff.
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(vec![Duration::ZERO]),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

        coordinator.kick();
        // Wait for permanent failure — after 1 retry (2 total attempts)
        wait_for_execution_status(db.as_ref(), &execution_id, "failed").await;

        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, "failed");
        assert_eq!(
            execution.pre_start_failure_count, 2,
            "expected 2 pre-start failures (initial + 1 retry); got {}",
            execution.pre_start_failure_count
        );

        let runs = db.list_runs(&execution_id).unwrap();
        assert_eq!(
            runs.len(),
            2,
            "expected 2 run rows (one per attempt); got {}",
            runs.len()
        );
        assert!(runs.iter().all(|r| r.status == "failed"));

        // Exactly one execution row — retries reuse the same row.
        let all_executions = db.list_executions(Some(&chore.id)).unwrap();
        assert_eq!(
            all_executions.len(),
            1,
            "retries must not create sibling execution rows; got {}",
            all_executions.len()
        );

        // Permanent failure surfaces exactly one attention item.
        let attention_items = db.list_attention_items(&execution_id).unwrap();
        assert_eq!(
            attention_items.len(),
            1,
            "permanent pre-start failure must raise exactly one attention item"
        );
        assert_eq!(attention_items[0].kind, "cube_workspace_lease_failed");
    }

    /// Pre-start retry: when the FIRST attempt fails but a second succeeds,
    /// the execution reaches `running` and only one execution row is created.
    #[tokio::test]
    async fn pre_start_failure_retries_and_succeeds_on_second_attempt() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Retry Then Succeed".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        // `lease_workspace_with_fallback` makes two `lease_workspace`
        // calls per dispatch attempt (primary + `any_free` fallback).
        // Fail both calls in the first attempt so the retry path
        // actually triggers; calls 3+ succeed.
        let cube = Arc::new(FakeCubeClient {
            fail_first_n_leases: 2,
            ..FakeCubeClient::default()
        });
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                // pending=true keeps the execution in `running` so we can
                // assert on it without racing against the WaitingHuman
                // transition.
                Arc::new(FakeExecutionRunner {
                    pending: true,
                    ..FakeExecutionRunner::default()
                }),
            )
            .with_pre_start_retry_delays(vec![Duration::ZERO]),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

        coordinator.kick();
        // On the retry the lease succeeds → execution reaches `running`.
        wait_for_execution_status(db.as_ref(), &execution_id, "running").await;

        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, "running");
        assert_eq!(
            execution.pre_start_failure_count, 1,
            "expected exactly 1 pre-start failure before the successful attempt; got {}",
            execution.pre_start_failure_count
        );

        // Only the one failed run row (from the initial attempt) + the active run.
        let runs = db.list_runs(&execution_id).unwrap();
        assert_eq!(
            runs.len(),
            2,
            "expected 1 failed run + 1 active run; got {}",
            runs.len()
        );

        // No attention items — the retry succeeded.
        let attention_items = db.list_attention_items(&execution_id).unwrap();
        assert!(
            attention_items.is_empty(),
            "successful retry must not surface an attention item"
        );

        // Exactly one execution row.
        let all_executions = db.list_executions(Some(&chore.id)).unwrap();
        assert_eq!(all_executions.len(), 1);
    }

    /// When `preferred_workspace_id` is set and cube refuses that workspace,
    /// the engine must NOT fall back to any other workspace — doing so would
    /// silently lose state continuity (the resuming worker needs that specific
    /// workspace). The dispatch must fail so the scheduler can retry with
    /// the correct workspace later.
    #[tokio::test]
    async fn lease_with_prefer_set_does_not_fall_back_when_refused() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();
        db.request_execution(RequestExecutionInput {
            work_item_id: chore.id.clone(),
            priority: None,
            preferred_workspace_id: Some("mono-agent-003".to_owned()),
            force: false,
        })
        .unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_lease_when_prefer_set: true,
            ..FakeCubeClient::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        // No retries: go straight to permanent failure to avoid backoff
        // delays and to keep the lease-call assertion at exactly 1.
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(vec![])
            .with_dispatch_events(recording.clone()),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, "failed").await;

        // Exactly one cube lease invocation: the engine must not retry
        // with a different workspace when a preferred workspace is set.
        let calls = cube.lease_calls.lock().await;
        assert_eq!(
            calls.len(),
            1,
            "engine must not retry when prefer is set; got {:?}",
            calls
        );
        assert_eq!(calls[0].2.as_deref(), Some("mono-agent-003"));
        drop(calls);

        let events = recording.events_for(&execution_id).await;
        let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();

        let attempt_events: Vec<&crate::dispatch_events::DispatchEvent> = events
            .iter()
            .filter(|e| e.stage == "cube_workspace_lease_attempted")
            .collect();
        assert_eq!(
            attempt_events.len(),
            1,
            "expected exactly one lease_attempted event; got stages {stages:?}"
        );
        assert_eq!(
            attempt_events[0]
                .details
                .get("prefer_workspace_id")
                .and_then(|v| v.as_str()),
            Some("mono-agent-003"),
        );
        assert_eq!(
            attempt_events[0]
                .details
                .get("fallback_policy")
                .and_then(|v| v.as_str()),
            Some("none"),
            "policy must be none when prefer is set — no silent workspace swap",
        );

        // Execution must fail, not succeed on a different workspace.
        assert!(
            !stages.contains(&"cube_workspace_leased"),
            "cube_workspace_leased must not appear; engine must not land on a different workspace; got {stages:?}",
        );

        let attention_items = db.list_attention_items(&execution_id).unwrap();
        assert_eq!(
            attention_items.len(),
            1,
            "terminal lease failure must raise exactly one attention item",
        );
        assert_eq!(attention_items[0].kind, "cube_workspace_lease_failed");
    }

    /// When `preferred_workspace_id=null` and cube fails the first workspace
    /// (e.g. because it has uncommitted work from a prior crashed lease),
    /// the engine must retry with `any_free` policy and land on the second
    /// workspace. This pins the fix for the 2026-05-12 dispatch failure
    /// where a single bad workspace blocked dispatch despite 12+ free ones.
    #[tokio::test]
    async fn lease_falls_back_when_no_prefer_and_first_workspace_refused() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();
        db.request_execution(RequestExecutionInput {
            work_item_id: chore.id.clone(),
            priority: None,
            preferred_workspace_id: None,
            force: false,
        })
        .unwrap();

        // First lease call fails (simulating a workspace with uncommitted
        // work refusing the reset); second call succeeds on a different
        // workspace.
        let cube = Arc::new(FakeCubeClient {
            fail_first_n_leases: 1,
            ..FakeCubeClient::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_dispatch_events(recording.clone()),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, "waiting_human").await;

        // Two cube lease invocations: first fails, second succeeds.
        let calls = cube.lease_calls.lock().await;
        assert_eq!(
            calls.len(),
            2,
            "engine must retry on any_free when no prefer set; got {:?}",
            calls
        );
        // Both calls have no --prefer (engine retries with same strategy).
        assert_eq!(calls[0].2, None);
        assert_eq!(calls[1].2, None);
        drop(calls);

        let events = recording.events_for(&execution_id).await;
        let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();

        // Timeline: attempted #1 → failed #1 → attempted #2 → leased.
        let attempt_events: Vec<&crate::dispatch_events::DispatchEvent> = events
            .iter()
            .filter(|e| e.stage == "cube_workspace_lease_attempted")
            .collect();
        assert_eq!(
            attempt_events.len(),
            2,
            "expected two lease_attempted events (initial + any_free retry); got stages {stages:?}"
        );
        assert_eq!(
            attempt_events[0]
                .details
                .get("fallback_policy")
                .and_then(|v| v.as_str()),
            Some("any_free"),
            "first attempt must carry any_free policy when no prefer set",
        );
        assert!(
            attempt_events[0]
                .details
                .get("prefer_workspace_id")
                .map(|v| v.is_null())
                .unwrap_or(false),
            "first attempt must have prefer_workspace_id=null; got {:?}",
            attempt_events[0].details,
        );
        assert_eq!(
            attempt_events[1]
                .details
                .get("fallback_policy")
                .and_then(|v| v.as_str()),
            Some("none"),
            "retry attempt has no further fallback",
        );

        let failed_events: Vec<&crate::dispatch_events::DispatchEvent> = events
            .iter()
            .filter(|e| e.stage == "cube_workspace_lease_failed")
            .collect();
        assert_eq!(
            failed_events.len(),
            1,
            "exactly one lease_failed event for the first attempt; got stages {stages:?}"
        );

        // Final state: a successful `cube_workspace_leased` event.
        let leased = events
            .iter()
            .find(|e| e.stage == "cube_workspace_leased")
            .expect("cube_workspace_leased event missing after any_free retry");
        assert_eq!(leased.outcome, "ok");

        // No attention item — the fallback succeeded.
        let attention_items = db.list_attention_items(&execution_id).unwrap();
        assert!(
            attention_items.iter().all(|a| a.kind != "cube_workspace_lease_failed"),
            "any_free success must not raise a lease-failure attention item; got {attention_items:?}",
        );
    }

    /// When `preferred_workspace_id=null` and both lease attempts fail, the
    /// execution must transition to `failed` with both
    /// `cube_workspace_lease_failed` events visible — silent wait is not OK.
    #[tokio::test]
    async fn lease_fallback_failure_transitions_execution_to_failed() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();
        db.request_execution(RequestExecutionInput {
            work_item_id: chore.id.clone(),
            priority: None,
            preferred_workspace_id: None,
            force: false,
        })
        .unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_lease: true,
            ..FakeCubeClient::default()
        });
        let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
        // No retries: go straight to permanent failure to keep the event
        // count assertions (2 attempts, 2 failures) unambiguous.
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(vec![])
            .with_dispatch_events(recording.clone()),
        );
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        coordinator.kick();
        wait_for_execution_status(db.as_ref(), &execution_id, "failed").await;

        let events = recording.events_for(&execution_id).await;
        let attempt_count = events
            .iter()
            .filter(|e| e.stage == "cube_workspace_lease_attempted")
            .count();
        let failed_count = events
            .iter()
            .filter(|e| e.stage == "cube_workspace_lease_failed")
            .count();
        assert_eq!(
            attempt_count, 2,
            "expected initial + any_free retry attempt events; got {events:?}"
        );
        assert_eq!(
            failed_count, 2,
            "expected one lease_failed event per attempt; got {events:?}"
        );

        let attention_items = db.list_attention_items(&execution_id).unwrap();
        assert_eq!(
            attention_items.len(),
            1,
            "terminal lease failure must raise exactly one attention item",
        );
        assert_eq!(attention_items[0].kind, "cube_workspace_lease_failed");
    }

    #[tokio::test]
    async fn change_creation_failure_marks_execution_failed_and_releases_workspace() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_create: true,
            ..FakeCubeClient::default()
        });
        // No retries: go straight to permanent failure to keep the
        // release_calls assertion (exactly "lease-1") unambiguous.
        let coordinator = Arc::new(
            ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            )
            .with_pre_start_retry_delays(vec![]),
        );
        coordinator.kick();
        wait_for_execution_status(
            db.as_ref(),
            &db.list_executions(Some(&chore.id)).unwrap()[0].id,
            "failed",
        )
        .await;

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        assert_eq!(execution.status, "failed");
        let run = db.list_runs(&execution.id).unwrap().pop().unwrap();
        assert_eq!(run.status, "failed");
        assert_eq!(run.error_text.as_deref(), Some("cube change create failed"));
        assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
        assert_eq!(coordinator.worker_pool().idle_count().await, 1);
    }

    #[tokio::test]
    async fn worker_pool_clamps_size_to_hard_cap() {
        let pool = WorkerPool::new(MAX_WORKER_POOL_SIZE + 4);
        assert_eq!(pool.capacity().await, MAX_WORKER_POOL_SIZE);
    }

    #[tokio::test]
    async fn worker_pool_prefers_workspace_affinity_over_random() {
        let pool = WorkerPool::new(2);

        // Take both slots at once so we can record distinct affinities
        // without depending on which slot random selection lands on.
        let w_a = pool.claim_worker("exec-a", None).await.unwrap();
        let w_b = pool.claim_worker("exec-b", None).await.unwrap();
        assert_ne!(w_a, w_b, "second claim must fill the other free slot");
        pool.release_worker(&w_a, Some("ws-a")).await;
        pool.release_worker(&w_b, Some("ws-b")).await;

        // Preferring ws-b must pick whichever worker recorded ws-b
        // affinity, even though random selection from the free pool
        // would otherwise be a coin flip.
        let claimed = pool.claim_worker("exec-c", Some("ws-b")).await.unwrap();
        assert_eq!(claimed, w_b);
        pool.release_worker(&claimed, Some("ws-b")).await;

        // Preferring an unknown workspace falls through to random
        // selection from the free pool — either worker is a valid pick.
        let fallback = pool.claim_worker("exec-d", Some("ws-unknown")).await.unwrap();
        assert!(fallback == w_a || fallback == w_b);
    }

    /// `worker-{N}` and slot N must round-trip 1:1. The
    /// engine-owns-slots refactor depends on this — the runner
    /// derives the pane slot it sends to the app from the worker
    /// id the coordinator handed it. A regression in either format
    /// or parse would silently re-introduce two independent
    /// numbering systems.
    #[test]
    fn worker_id_and_slot_id_round_trip() {
        for slot in 1u8..=8 {
            let worker_id = WorkerPool::worker_id_for_slot(slot);
            assert_eq!(worker_id, format!("worker-{slot}"));
            assert_eq!(slot_id_from_worker_id(&worker_id), Some(slot));
        }
    }

    #[test]
    fn slot_id_from_worker_id_rejects_garbage() {
        assert_eq!(slot_id_from_worker_id(""), None);
        assert_eq!(slot_id_from_worker_id("worker"), None);
        assert_eq!(slot_id_from_worker_id("worker-"), None);
        assert_eq!(slot_id_from_worker_id("worker-0"), None);
        assert_eq!(slot_id_from_worker_id("worker-abc"), None);
        assert_eq!(slot_id_from_worker_id("agent-1"), None);
    }

    #[tokio::test]
    async fn worker_pool_random_fallback_spreads_across_free_slots() {
        // With M free slots and N >> M claims, every slot should be
        // hit at least once. This is the cosmetic guarantee the
        // randomization is for: don't always start at slot 1.
        let pool_size = 4;
        let trials = 200;
        let pool = WorkerPool::new(pool_size);
        let mut hits = vec![0usize; pool_size];
        for i in 0..trials {
            let claimed = pool
                .claim_worker(&format!("exec-{i}"), None)
                .await
                .unwrap();
            let slot: usize = claimed
                .strip_prefix("worker-")
                .unwrap()
                .parse()
                .unwrap();
            hits[slot - 1] += 1;
            pool.release_worker(&claimed, None).await;
        }
        for (slot, count) in hits.iter().enumerate() {
            assert!(
                *count > 0,
                "slot worker-{} was never picked across {trials} claims",
                slot + 1
            );
        }
    }

    #[tokio::test]
    async fn higher_priority_executions_run_first() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let early = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Old".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let late = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "New".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        // Bump the later chore's priority — it should run first despite
        // the older one being in the queue first.
        db.request_execution(RequestExecutionInput {
            work_item_id: late.id.clone(),
            priority: Some(10),
            preferred_workspace_id: None,
            force: false,
        })
        .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            runner.clone(),
        ));
        coordinator.kick();

        for _ in 0..100 {
            let runs = runner.calls.lock().await;
            if !runs.is_empty() {
                break;
            }
            drop(runs);
            sleep(Duration::from_millis(10)).await;
        }

        let calls = runner.calls.lock().await;
        assert!(!calls.is_empty(), "scheduler did not start any run");
        let started_execution_id = &calls[0].1;
        let late_execution = db
            .list_executions(Some(&late.id))
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            started_execution_id, &late_execution.id,
            "expected the higher-priority chore to run first"
        );
        // Old chore should still be queued (and was NOT picked).
        let early_execution = db
            .list_executions(Some(&early.id))
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(early_execution.status, "ready");
    }

    #[tokio::test]
    async fn scheduler_passes_preferred_workspace_to_lease_and_records_affinity() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();
        db.request_execution(RequestExecutionInput {
            work_item_id: chore.id.clone(),
            priority: None,
            preferred_workspace_id: Some("mono-agent-007".to_owned()),
            force: false,
        })
        .unwrap();

        let cube = Arc::new(FakeCubeClient::default().with_next_workspace_id("mono-agent-007"));
        let runner = Arc::new(FakeExecutionRunner::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            runner.clone(),
        ));
        coordinator.kick();

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        wait_for_execution_status(db.as_ref(), &execution.id, "waiting_human").await;

        let calls = cube.lease_calls.lock().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].2.as_deref(), Some("mono-agent-007"));
        drop(calls);

        let execution = db.get_execution(&execution.id).unwrap();
        assert_eq!(
            execution.cube_workspace_id.as_deref(),
            Some("mono-agent-007")
        );
        assert_eq!(
            coordinator
                .worker_pool()
                .worker_affinity("worker-1")
                .await
                .as_deref(),
            Some("mono-agent-007")
        );
    }

    #[tokio::test]
    async fn coordinator_publishes_execution_topic_events() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let coordinator = Arc::new(ExecutionCoordinator::with_publisher(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
            publisher.clone(),
        ));
        coordinator.kick();

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        wait_for_execution_status(db.as_ref(), &execution.id, "waiting_human").await;

        let events = publisher.events.lock().await;
        let reasons: Vec<&str> = events
            .iter()
            .map(|(_, _, _, reason)| reason.as_str())
            .collect();
        assert!(reasons.contains(&"execution_started"));
        assert!(reasons.contains(&"execution_run_completed"));
        let last_status = events
            .iter()
            .rev()
            .find(|(_, _, _, reason)| reason == "execution_run_completed")
            .map(|(_, _, status, _)| status.clone());
        assert_eq!(last_status.as_deref(), Some("waiting_human"));

        // The kanban activity-icon depends on a work-tree invalidation
        // on run completion, otherwise the card would stay stuck on
        // "active" after the agent moved to waiting_human. Confirm the
        // coordinator now fires the broadcast on the completion path
        // too — not just on execution-start auto-advance.
        let work_item_events = publisher.work_item_events.lock().await;
        assert!(
            work_item_events.iter().any(|(_, _, reason)| {
                reason == "execution_run_completed"
            }),
            "expected execution_run_completed work-item invalidation, got: {:?}",
            *work_item_events,
        );
    }

    /// When `start_execution_run` auto-advances `tasks.status` to
    /// `'active'`, the coordinator must also publish a work-tree
    /// invalidation so kanban subscribers re-fetch the board. Without
    /// this, the DB has the right value but the GUI never refreshes
    /// — the bug surfaced manually that this test exists to prevent.
    #[tokio::test]
    async fn coordinator_publishes_work_item_changed_on_execution_start() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let coordinator = Arc::new(ExecutionCoordinator::with_publisher(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
            publisher.clone(),
        ));
        coordinator.kick();

        let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
        wait_for_execution_status(db.as_ref(), &execution.id, "waiting_human").await;

        // Work-item invalidation should have fired with the chore's
        // product id and the chore's work-item id. Reason wording
        // isn't load-bearing but we assert it's there to confirm the
        // call site is the auto-advance one and not some unrelated
        // future broadcast.
        let work_item_events = publisher.work_item_events.lock().await;
        assert!(
            work_item_events.iter().any(|(product_id, work_item_id, reason)| {
                product_id == &product.id
                    && work_item_id == &chore.id
                    && reason == "execution_started_auto_advance"
            }),
            "expected execution_started_auto_advance event for chore {} on product {}, got: {:?}",
            chore.id,
            product.id,
            *work_item_events,
        );

        // And the DB-level auto-advance itself: the chore status must
        // have flipped from `todo` to `active` when the execution
        // started running.
        let advanced = db.get_work_item(&chore.id).unwrap();
        match advanced {
            WorkItem::Chore(t) | WorkItem::Task(t) => {
                assert_eq!(t.status, "active", "chore should auto-advance to active");
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn scheduler_respects_worker_pool_capacity() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let first_project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Design A".to_owned(),
                description: None,
                goal: None,
                autostart: true,
                no_design_task: false,
            })
            .unwrap();
        let second_project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Design B".to_owned(),
                description: None,
                goal: None,
                autostart: true,
                no_design_task: false,
            })
            .unwrap();
        db.create_task(CreateTaskInput {
            product_id: product.id.clone(),
            project_id: first_project.id.clone(),
            name: "A1".to_owned(),
            description: None,
            autostart: true,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        })
        .unwrap();
        db.create_task(CreateTaskInput {
            product_id: product.id.clone(),
            project_id: second_project.id.clone(),
            name: "B1".to_owned(),
            description: None,
            autostart: true,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        })
        .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        ));
        coordinator.kick();
        for _ in 0..100 {
            let executions = db.list_executions(None).unwrap();
            if executions
                .iter()
                .filter(|execution| execution.status == "running")
                .count()
                == 1
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        let executions = db.list_executions(None).unwrap();
        assert_eq!(
            executions
                .iter()
                .filter(|execution| execution.status == "running")
                .count(),
            1,
            "pool cap = 1 must keep exactly one execution `running`",
        );
        // Project design now lives on a per-project `kind = 'design'`
        // task at `ordinal = 0`, with the user's project_tasks at
        // `ordinal >= 1`. Only the design tasks are eligible for
        // `ready` until they complete; the user-tasks stay
        // `waiting_dependency` behind their project's design. So the
        // shape is: 1 running design, 1 ready design (gated on the
        // pool slot), 2 waiting_dependency project_tasks.
        assert_eq!(
            executions
                .iter()
                .filter(|execution| execution.status == "ready")
                .count(),
            1,
        );
        assert_eq!(
            executions
                .iter()
                .filter(|execution| execution.status == "waiting_dependency")
                .count(),
            2,
        );
        assert_eq!(coordinator.worker_pool().idle_count().await, 0);
    }

    /// Ghost-active regression: when the worker pool is exhausted,
    /// chores that lost the dispatcher's claim race must NOT have
    /// `tasks.status` flipped to `'active'`. They stay in `todo` so
    /// `boss chore list --status active` and `bossctl agents list`
    /// agree on which chores actually have a worker.
    ///
    /// Setup: pool capped at 1, three autostart chores reconciled into
    /// `ready` executions back-to-back. Only one can be dispatched —
    /// the other two must remain `todo` with no run record. This is
    /// the test that would have caught the "6 active, 4 workers"
    /// observation in the bug report.
    #[tokio::test]
    async fn pool_exhaustion_does_not_ghost_activate_chores() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();

        let mut chore_ids = Vec::new();
        for index in 0..3 {
            let chore = db
                .create_chore(CreateChoreInput {
                    product_id: product.id.clone(),
                    name: format!("Chore {index}"),
                    description: None,
                    autostart: true,
                    priority: None,
                    created_via: None,
                    repo_remote_url: None,
                    effort_level: None,
                    model_override: None,
                    force_duplicate: false,
                })
                .unwrap();
            chore_ids.push(chore.id);
        }
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        ));
        coordinator.kick();

        // Wait for the dispatcher to settle on exactly one running
        // execution. With pool=1 and 3 ready chores the loop must
        // claim the first slot, then break on pool exhaustion.
        for _ in 0..200 {
            let executions = db.list_executions(None).unwrap();
            if executions
                .iter()
                .filter(|execution| execution.status == "running")
                .count()
                == 1
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        // One chore active with a run, two stay todo with no run.
        let mut active_with_run = 0usize;
        let mut still_todo = 0usize;
        for chore_id in &chore_ids {
            let item = db.get_work_item(chore_id).unwrap();
            let status = match item {
                WorkItem::Chore(t) | WorkItem::Task(t) => t.status,
                other => panic!("expected chore/task, got {other:?}"),
            };
            let executions = db.list_executions(Some(chore_id)).unwrap();
            assert_eq!(executions.len(), 1, "exactly one execution per chore");
            let runs = db.list_runs(&executions[0].id).unwrap();
            match status.as_str() {
                "active" => {
                    assert_eq!(executions[0].status, "running");
                    assert_eq!(runs.len(), 1, "active chore must have a run record");
                    assert_eq!(runs[0].status, "active");
                    active_with_run += 1;
                }
                "todo" => {
                    assert_eq!(executions[0].status, "ready");
                    assert!(
                        runs.is_empty(),
                        "todo chore must not have a run record yet, got {runs:?}",
                    );
                    still_todo += 1;
                }
                other => panic!(
                    "chore {chore_id} unexpectedly in status `{other}` — \
                     `active` and `todo` are the only valid states for this \
                     pool-exhausted scenario",
                ),
            }
        }
        assert_eq!(
            active_with_run, 1,
            "exactly one chore should be active with a run; got {active_with_run}",
        );
        assert_eq!(
            still_todo, 2,
            "two chores should stay `todo` with no run; got {still_todo}",
        );
        assert_eq!(coordinator.worker_pool().idle_count().await, 0);
    }

    /// Boot-time heal: a `tasks.status = 'active'` row whose
    /// executions never produced a `work_runs` entry (e.g. previous
    /// engine crashed between the kanban drag and the dispatch claim,
    /// or a `RequestExecution` raced ahead of an exhausted pool) is
    /// demoted back to `todo` on startup. Items WITH run history are
    /// left alone — `reconcile_active_dispatch` is the right tool for
    /// those.
    #[tokio::test]
    async fn heal_ghost_active_demotes_chores_without_run_history() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();

        // Ghost A: dragged to Doing but no execution exists at all.
        let ghost_a = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Ghost A".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.update_work_item(
            &ghost_a.id,
            crate::work::WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();

        // Ghost B: dragged to Doing, has a `ready` execution but no
        // run yet — the "RequestExecution raced an exhausted pool"
        // shape from the bug report.
        let ghost_b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Ghost B".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.update_work_item(
            &ghost_b.id,
            crate::work::WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        db.request_execution(RequestExecutionInput {
            work_item_id: ghost_b.id.clone(),
            priority: None,
            preferred_workspace_id: None,
            force: false,
        })
        .unwrap();

        // Real worker: started a run before the engine restarted,
        // mimicking a crashed-mid-flight chore. heal must NOT touch
        // this — `reconcile_active_dispatch` redispatches it.
        let real = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Real worker".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        let real_exec = db
            .create_execution(crate::work::CreateExecutionInput {
                work_item_id: real.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
                prefer_is_soft: false,
                pr_url: None,
            })
            .unwrap();
        db.start_execution_run(
            &real_exec.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();

        let healed = db.heal_ghost_active_chores().unwrap();
        let mut healed_ids: Vec<String> =
            healed.iter().map(|h| h.work_item_id.clone()).collect();
        healed_ids.sort();
        let mut expected = vec![ghost_a.id.clone(), ghost_b.id.clone()];
        expected.sort();
        assert_eq!(healed_ids, expected, "healed only the ghost rows");
        // product_id rides along so the caller can publish a
        // work-item-changed event on the product's kanban topic.
        for h in &healed {
            assert_eq!(
                h.product_id, product.id,
                "healed row should carry its product_id"
            );
        }

        // Demoted ghosts now sit in `todo` and are stamped as engine-
        // initiated so the kanban can attribute the move correctly
        // instead of blaming the human who last dragged the row.
        for id in &[&ghost_a.id, &ghost_b.id] {
            match db.get_work_item(id).unwrap() {
                WorkItem::Chore(t) | WorkItem::Task(t) => {
                    assert_eq!(t.status, "todo");
                    assert_eq!(t.last_status_actor, "engine");
                }
                other => panic!("expected chore/task, got {other:?}"),
            }
        }

        // Ghost B's stranded `ready` execution was abandoned so the
        // dispatcher won't claim a slot for a chore that just got
        // pulled out of the Doing column.
        let ghost_b_execs = db.list_executions(Some(&ghost_b.id)).unwrap();
        assert_eq!(ghost_b_execs.len(), 1);
        assert_eq!(ghost_b_execs[0].status, "abandoned");

        // The real chore stays `active` with its `running` execution
        // intact — heal is conservative.
        match db.get_work_item(&real.id).unwrap() {
            WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, "active"),
            other => panic!("expected chore/task, got {other:?}"),
        }
        let real_execs = db.list_executions(Some(&real.id)).unwrap();
        assert_eq!(real_execs.len(), 1);
        assert_eq!(real_execs[0].status, "running");
    }

    /// Regression coverage for PR #228. Default-sized pool
    /// (`MAX_WORKER_POOL_SIZE` = 8) must dispatch all five chores when
    /// they autostart back-to-back — the original bug was a pool that
    /// silently capped at 1 (and an earlier-still incarnation that
    /// capped at 4), so `kick()` broke out of `run_scheduler` after
    /// claiming the first few workers and the rest stayed `ready`.
    /// This test would have caught that: it asserts every one of the
    /// five executions reaches `running`, and that the pool consumed
    /// five distinct worker slots (so dispatch fanned out into the
    /// 5..=8 range that the original bug had unreachable).
    #[tokio::test]
    async fn default_pool_dispatches_five_concurrent_autostart_chores() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();

        // Five autostart chores — the same shape `boss chore create`
        // produces when `--no-autostart` is omitted. Reconcile then
        // promotes each to a `ready` execution row.
        for index in 0..5 {
            db.create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: format!("Chore {index}"),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        }
        db.reconcile_product_executions(&product.id).unwrap();

        // Use the default pool size so this test pins the contract
        // `WorkConfig::load_from_env` exposes to production.
        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(MAX_WORKER_POOL_SIZE),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        ));
        coordinator.kick();

        for _ in 0..200 {
            let executions = db.list_executions(None).unwrap();
            if executions
                .iter()
                .filter(|execution| execution.status == "running")
                .count()
                == 5
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        let executions = db.list_executions(None).unwrap();
        let running = executions
            .iter()
            .filter(|execution| execution.status == "running")
            .count();
        assert_eq!(
            running, 5,
            "expected all 5 autostart chores to be dispatched concurrently, got {running} running",
        );
        assert_eq!(coordinator.worker_pool().idle_count().await, 3);
    }

    /// `bossctl agents launch` (Phase 7 of the v2 plan) must dispatch
    /// even when every configured slot is busy — the verb's whole point
    /// is to *skip the queue*. We mirror the cap test above
    /// (`scheduler_respects_worker_pool_capacity`) but with a smaller
    /// pool so we can sit under the hard cap, fill every slot, and
    /// then prove `force_dispatch` grows the pool by one slot and runs
    /// the launched item immediately rather than leaving it `ready`.
    #[tokio::test]
    async fn force_dispatch_bypasses_configured_pool_cap() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let busy = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Already running".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        // A second chore that will sit in `ready` because the
        // configured pool size is 1 and `busy` claimed it.
        let queued = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Skip the queue".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        ));
        coordinator.kick();

        // Wait for the first chore to actually be claimed by the lone
        // worker slot — otherwise force_dispatch might race the
        // scheduler and grow the pool unnecessarily.
        for _ in 0..200 {
            let busy_exec = db.list_executions(Some(&busy.id)).unwrap().pop().unwrap();
            if busy_exec.status == "running" {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(coordinator.worker_pool().idle_count().await, 0);
        assert_eq!(coordinator.worker_pool().capacity().await, 1);

        // `bossctl agents launch <queued.id>` enters the engine via
        // `RequestExecution { force: true }`. Promote `queued` to a
        // `ready` execution (the auto-start opt-out kept it parked),
        // then call the same coordinator entry point that `app.rs`
        // hits when `force = true`.
        let queued_exec = db
            .request_execution(RequestExecutionInput {
                work_item_id: queued.id.clone(),
                priority: None,
                preferred_workspace_id: None,
                force: true,
            })
            .unwrap();
        let worker_id = coordinator
            .force_dispatch(&queued_exec.id)
            .await
            .expect("force_dispatch should bypass the cap and return a worker id");
        assert_eq!(
            worker_id, "worker-2",
            "expected force_dispatch to grow the pool with a new slot",
        );

        for _ in 0..200 {
            let queued_after = db
                .list_executions(Some(&queued.id))
                .unwrap()
                .pop()
                .unwrap();
            if queued_after.status == "running" {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        let queued_after = db
            .list_executions(Some(&queued.id))
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            queued_after.status, "running",
            "force-launched execution should be dispatched immediately",
        );
        assert_eq!(
            coordinator.worker_pool().capacity().await,
            2,
            "force_dispatch must grow the pool by one slot",
        );
        assert_eq!(coordinator.worker_pool().idle_count().await, 0);
    }

    /// The pool-grow path is hard-capped at `MAX_WORKER_POOL_SIZE`
    /// because the macOS app only has eight panes. A force-launch
    /// request that arrives with every hard-cap slot busy must surface
    /// a real error instead of silently overcommitting.
    /// On-free rescan regression: a chore whose `tasks.status` is
    /// `active` but whose latest execution is terminal (worker died,
    /// cube lease errored, kanban-drag-while-pool-was-full) must be
    /// redispatched the next time a worker frees up. Without the
    /// rescan, `kick()` only sees `ready` executions and the stuck
    /// chore stays in Doing forever.
    #[tokio::test]
    async fn worker_release_redispatches_active_chore_with_terminal_execution() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();

        // Warm-up chore: gets a normal `ready` execution so the
        // dispatcher has something to consume the single pool slot.
        // Its run completes via FakeExecutionRunner (WaitingHuman), at
        // which point the pool worker is released and our rescan fires.
        let warm = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Warm-up".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        // Stuck chore: `active` with a `failed` execution row,
        // mimicking the bug — worker died, kanban card stayed in
        // Doing, and the create-time dispatch path won't ever look
        // at it again.
        let stuck = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Stuck".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.update_work_item(
            &stuck.id,
            crate::work::WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        db.create_execution(crate::work::CreateExecutionInput {
            work_item_id: stuck.id.clone(),
            kind: "chore_implementation".to_owned(),
            status: Some("failed".to_owned()),
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            cube_repo_id: None,
            cube_lease_id: None,
            cube_workspace_id: None,
            workspace_path: None,
            priority: None,
            preferred_workspace_id: None,
            started_at: None,
            finished_at: None,
            prefer_is_soft: false,
            pr_url: None,
        })
        .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        ));
        coordinator.kick();

        // Wait for the stuck chore to reach a non-failed execution
        // — that means the rescan inserted a fresh `ready` row and
        // the post-release `kick()` claimed it.
        for _ in 0..400 {
            let executions = db.list_executions(Some(&stuck.id)).unwrap();
            if executions
                .iter()
                .any(|exec| matches!(exec.status.as_str(), "running" | "waiting_human"))
            {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }

        let warm_execs = db.list_executions(Some(&warm.id)).unwrap();
        let stuck_execs = db.list_executions(Some(&stuck.id)).unwrap();
        panic!(
            "stuck chore was never redispatched after warm-up release;\nwarm executions: {warm_execs:?}\nstuck executions: {stuck_execs:?}",
        );
    }

    /// Negative case for the rescan: an `autostart=false` chore that
    /// is parked in `active` with a terminal execution must remain
    /// untouched even after a worker frees up. The on-free rescan is
    /// recurring; without the autostart filter it would loop on a
    /// chore the user explicitly opted out of auto-handling.
    #[tokio::test]
    async fn worker_release_skips_no_autostart_active_chore() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();

        let warm = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Warm-up".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let parked = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Parked".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.update_work_item(
            &parked.id,
            crate::work::WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        db.create_execution(crate::work::CreateExecutionInput {
            work_item_id: parked.id.clone(),
            kind: "chore_implementation".to_owned(),
            status: Some("failed".to_owned()),
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            cube_repo_id: None,
            cube_lease_id: None,
            cube_workspace_id: None,
            workspace_path: None,
            priority: None,
            preferred_workspace_id: None,
            started_at: None,
            finished_at: None,
            prefer_is_soft: false,
            pr_url: None,
        })
        .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        ));
        coordinator.kick();

        // Wait for the warm-up to settle (its run will finish on
        // WaitingHuman). After that the rescan has had its chance to
        // touch the parked chore — it must not have.
        wait_for_execution_status(
            db.as_ref(),
            &db.list_executions(Some(&warm.id)).unwrap()[0].id,
            "waiting_human",
        )
        .await;
        // Give the post-release rescan a clear window in which to
        // (incorrectly) redispatch the parked chore. 100ms is plenty
        // — the rescan is synchronous on the release path.
        sleep(Duration::from_millis(100)).await;

        let parked_execs = db.list_executions(Some(&parked.id)).unwrap();
        assert_eq!(
            parked_execs.len(),
            1,
            "autostart=false parked chore must not be redispatched, got {parked_execs:?}",
        );
        assert_eq!(parked_execs[0].status, "failed");
    }

    #[tokio::test]
    async fn force_dispatch_errors_at_hard_cap() {
        let pool = WorkerPool::new(MAX_WORKER_POOL_SIZE);
        for i in 0..MAX_WORKER_POOL_SIZE {
            pool.claim_worker(&format!("exec-{i}"), None)
                .await
                .expect("hard-cap pool should hand out one slot per claim");
        }
        assert_eq!(pool.idle_count().await, 0);
        assert!(
            pool.claim_worker_force("overflow", None).await.is_none(),
            "claim_worker_force must reject when the pool is already at the hard cap",
        );
        assert_eq!(
            pool.capacity().await,
            MAX_WORKER_POOL_SIZE,
            "rejected force-claim must not grow the pool past the hard cap",
        );
    }

    /// Regression for `task_18ae9d21044843b8_44` — `bossctl work start`
    /// returned `status: ready` but no scheduler ever ran, leaving the
    /// row stranded. Root cause was a TOCTOU between the scheduler's
    /// last `list_ready_executions()` call and dropping its
    /// `scheduling_active` guard: a `kick()` that landed in that
    /// window observed `active=true`, returned without spawning, and
    /// the guard then dropped to `false` with no scheduler running.
    ///
    /// The fix latches every `kick()` into `scheduling_pending` so the
    /// alive scheduler always notices the wakeup. This test pins the
    /// contract: a `kick()` that arrives while `scheduling_active` is
    /// already true MUST set `scheduling_pending` so the running
    /// scheduler can re-enter its drain loop.
    #[tokio::test]
    async fn kick_during_active_scheduler_latches_pending_wakeup() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db,
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        ));

        // Simulate "another scheduler is already running".
        coordinator
            .scheduling_active
            .store(true, Ordering::Release);
        coordinator
            .scheduling_pending
            .store(false, Ordering::Release);

        coordinator.kick();

        assert!(
            coordinator.scheduling_pending.load(Ordering::Acquire),
            "kick that lost the active-flag race must still latch pending so the alive \
             scheduler re-enters its drain loop instead of exiting on stale state",
        );
    }

    /// End-to-end regression for the same race: even when a `kick()`
    /// loses the active-flag race, the row it queued for must still
    /// reach a worker. We can't deterministically force the OS into
    /// the exact "scheduler just finished its drain" timing, but we
    /// can prove the contract works by simulating the surviving
    /// scheduler picking up the wakeup: the pending bit is the
    /// in-process signal; if the pending bit is honored on the next
    /// run_scheduler entry, the new row gets processed.
    #[tokio::test]
    async fn ready_row_added_during_active_window_still_dispatches() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Stranded by lost wakeup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        ));

        // Simulate the bug-trigger sequence:
        //   1. A previous scheduler is "alive" (active=true) but
        //      has already finished its drain.
        //   2. RequestExecution lands, inserts a ready row, calls
        //      kick(). With the old code: kick observes active=true,
        //      returns, and the (now-exiting) scheduler drops the
        //      guard without re-checking. New row stranded.
        //   3. With the fix: kick latches pending=true.
        coordinator
            .scheduling_active
            .store(true, Ordering::Release);
        coordinator
            .scheduling_pending
            .store(false, Ordering::Release);
        coordinator.kick(); // noop on `active`, but latches pending

        // Now simulate the previous scheduler exiting: it must
        // honour the pending bit. Drop `active` and re-enter
        // `run_scheduler` exactly as the lossless-wakeup logic
        // would on the post-drain re-check path.
        coordinator
            .scheduling_active
            .store(false, Ordering::Release);
        assert!(
            coordinator.scheduling_pending.load(Ordering::Acquire),
            "post-drain re-check must see pending=true so the new row is not lost",
        );

        // The fix re-claims `active` and re-enters the drain. Kick
        // again to simulate that re-entry (this is what the
        // post-drain block in `run_scheduler` does internally), and
        // assert the row reaches `waiting_human`.
        coordinator.kick();
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
        wait_for_execution_status(db.as_ref(), &execution_id, "waiting_human").await;
    }

    /// Regression for the 2026-05-12 "`@` got re-pointed mid-flight"
    /// incident (`mono-agent-001`, Worf's report). Pre-fix, the engine
    /// never called `cube_client.heartbeat_lease` from anywhere — the
    /// trait method had only stub implementations in test mocks. Any
    /// worker that ran longer than `DEFAULT_LEASE_TTL_SECS = 1800` had
    /// its lease silently age out, after which the next
    /// `cube workspace lease` call from another execution reclaimed
    /// the workspace and ran `jj new <main>` on the still-active
    /// worker's working copy.
    ///
    /// This test pins down the fix: while the guard is alive, the
    /// heartbeat fires at the configured interval; dropping the guard
    /// stops the heartbeat. The default 5-minute production interval
    /// is shortened to 50 ms here so the test stays fast.
    #[tokio::test]
    async fn heartbeat_guard_renews_lease_until_dropped() {
        use super::{HeartbeatGuard, LocalHostAdapter};
        use crate::host_adapter::HostAdapter;

        let cube = Arc::new(FakeCubeClient::default());
        // Thin shim: wrap the FakeCubeClient in a LocalHostAdapter so the
        // HostAdapter-typed HeartbeatGuard interface is satisfied. The test
        // still inspects heartbeat_calls on the inner FakeCubeClient.
        let adapter: Arc<dyn HostAdapter> = Arc::new(LocalHostAdapter::new(
            cube.clone() as Arc<dyn CubeClient>,
            Arc::new(FakeExecutionRunner::default()),
        ));
        let guard = HeartbeatGuard::spawn_with_interval(
            adapter,
            "lease-1".to_owned(),
            "exec-1".to_owned(),
            "run-1".to_owned(),
            "worker-1".to_owned(),
            Duration::from_millis(50),
        );

        // Three intervals: expect at least two heartbeats (the first
        // tick is consumed at startup so the timer measures gaps).
        sleep(Duration::from_millis(180)).await;
        let beats_during = cube.heartbeat_calls.lock().await.len();
        assert!(
            beats_during >= 2,
            "expected >= 2 heartbeats in ~180ms with a 50ms interval, got {beats_during}",
        );
        for (lease, ttl) in cube.heartbeat_calls.lock().await.iter() {
            assert_eq!(lease, "lease-1");
            assert!(ttl.is_none(), "engine heartbeats use cube's default TTL");
        }

        // Drop stops the task. Sleep through more intervals and
        // assert the count is frozen — proving the heartbeat is
        // scoped to the guard's lifetime and cannot extend a lease
        // the run has already finished with.
        drop(guard);
        sleep(Duration::from_millis(50)).await;
        let beats_after_drop_snapshot = cube.heartbeat_calls.lock().await.len();
        sleep(Duration::from_millis(200)).await;
        let beats_final = cube.heartbeat_calls.lock().await.len();
        assert_eq!(
            beats_final, beats_after_drop_snapshot,
            "heartbeat must stop firing after the guard is dropped",
        );
    }

    /// Regression for `exec_18af3ba5259d32a8_12` (2026-05-13): a `ready`
    /// execution row that misses its scheduler wakeup sits at
    /// `status_transition` until the 90s-age orphan-active reconciler
    /// rescues it. With the heartbeat installed, the same stranded row
    /// reaches a worker within one heartbeat interval — no abandon /
    /// redispatch needed.
    ///
    /// The test simulates the failure mode by inserting a `ready` row
    /// without calling `kick()`, then spawning the heartbeat with a
    /// short interval. The heartbeat must observe the stranded row
    /// (the "fail loudly" surface for operators) and re-kick so the
    /// scheduler drains it.
    #[tokio::test]
    async fn heartbeat_rekicks_when_ready_row_was_orphaned_by_a_dropped_kick() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Stranded by lost wakeup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        // Inserts a `ready` execution row but does NOT call `kick()`.
        // This mirrors the post-mortem evidence: the row exists, the
        // status_transition event was written, but no scheduler ever
        // picked the row up.
        db.reconcile_product_executions(&product.id).unwrap();
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        ));

        // Confirm the precondition: the row is `ready` and no scheduler
        // is running. (No `kick()` has been called.)
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            "ready",
            "precondition: row must be `ready` before the heartbeat fires",
        );

        // Install the heartbeat with a short interval so the test
        // doesn't have to sleep for 15s of production cadence. The
        // heartbeat's startup-stagger sleep also uses this interval.
        let _handle = coordinator.spawn_scheduler_heartbeat(Duration::from_millis(80));

        // Within a few intervals the heartbeat should kick the
        // scheduler, drain the row, and move it through to
        // `waiting_human` via the fake runner.
        wait_for_execution_status(db.as_ref(), &execution_id, "waiting_human").await;
    }

    /// `stranded_ready_executions` is the read-side helper the heartbeat
    /// uses to surface dropped-wakeup symptoms. This test pins its
    /// contract directly so the heartbeat's `warn!` line is asserted on
    /// without depending on timer behaviour: a row younger than the
    /// configured threshold is invisible to the helper; once the row
    /// crosses the threshold it appears with its actual age.
    #[tokio::test]
    async fn stranded_ready_executions_only_returns_rows_past_the_threshold() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Age boundary".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();
        let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        ));

        // Threshold far in the future: the freshly-inserted row is too
        // young to count as stranded.
        let fresh = coordinator.stranded_ready_executions(60_000);
        assert!(
            fresh.is_empty(),
            "row younger than the threshold must not be flagged as stranded: {fresh:?}",
        );

        // Threshold of zero: any ready row should appear. The
        // execution we just inserted is in the queue with age >= 0.
        let any = coordinator.stranded_ready_executions(0);
        assert!(
            any.iter().any(|(id, _)| id == &execution_id),
            "with min_age_ms=0 the helper must surface the freshly-inserted ready row; \
             got {any:?}",
        );
    }
}
