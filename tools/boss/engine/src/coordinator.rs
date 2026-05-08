use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::config::RuntimeConfig;
use crate::runner::{ExecutionRunner, RunOutcome};
use crate::work::{CreateAttentionItemInput, WorkDb, WorkExecution, WorkItem, WorkRun};

/// Hard cap on the worker pool. The runtime config can request a smaller
/// pool, but values above this are clamped (with a warning). The V2
/// design fixes 8 as the upper bound.
pub const MAX_WORKER_POOL_SIZE: usize = 8;

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
}

#[derive(Debug, Clone)]
pub struct CommandCubeClient {
    cfg: Arc<RuntimeConfig>,
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

    #[cfg(test)]
    async fn idle_count(&self) -> usize {
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
}

#[derive(Default)]
pub struct NoopExecutionPublisher;

#[async_trait]
impl ExecutionPublisher for NoopExecutionPublisher {
    async fn publish(&self, _: &str, _: &str, _: &str, _: &str) {}
    async fn publish_work_item_changed(&self, _: &str, _: &str, _: &str) {}
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
    cube_client: Arc<dyn CubeClient>,
    execution_runner: Arc<dyn ExecutionRunner>,
    publisher: Arc<dyn ExecutionPublisher>,
    scheduling_active: AtomicBool,
}

impl ExecutionCoordinator {
    pub fn new(
        work_db: Arc<WorkDb>,
        worker_pool: WorkerPool,
        cube_client: Arc<dyn CubeClient>,
        execution_runner: Arc<dyn ExecutionRunner>,
    ) -> Self {
        Self::with_publisher(
            work_db,
            worker_pool,
            cube_client,
            execution_runner,
            Arc::new(NoopExecutionPublisher::default()),
        )
    }

    pub fn with_publisher(
        work_db: Arc<WorkDb>,
        worker_pool: WorkerPool,
        cube_client: Arc<dyn CubeClient>,
        execution_runner: Arc<dyn ExecutionRunner>,
        publisher: Arc<dyn ExecutionPublisher>,
    ) -> Self {
        Self {
            work_db,
            worker_pool,
            cube_client,
            execution_runner,
            publisher,
            scheduling_active: AtomicBool::new(false),
        }
    }

    pub fn worker_pool(&self) -> WorkerPool {
        self.worker_pool.clone()
    }

    pub fn kick(self: &Arc<Self>) {
        if self.scheduling_active.swap(true, Ordering::AcqRel) {
            return;
        }
        let coordinator = self.clone();
        tokio::spawn(async move {
            coordinator.run_scheduler().await;
        });
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
        let _guard = SchedulingGuard {
            active: &self.scheduling_active,
        };

        // Free-up redispatch: a work item that's still `active` (in
        // the kanban Doing column) but whose latest execution is
        // terminal has nothing the ready-queue scan below will pick
        // up. The card just sits there until a human nudges it. Run
        // a conservative scan now — only items with terminal/absent
        // executions get a fresh `ready` row, so we never race a
        // running or waiting_human execution that genuinely owns its
        // worker. Startup recovery still owns the
        // stale-`waiting_human`-after-crash path via
        // reconcile_active_dispatch.
        match self.work_db.redispatch_terminated_active_items() {
            Ok(redispatched) if !redispatched.is_empty() => {
                tracing::info!(
                    count = redispatched.len(),
                    ids = ?redispatched,
                    "redispatched stranded active items on scheduler entry",
                );
            }
            Ok(_) => {}
            Err(err) => {
                tracing::error!(?err, "free-up redispatch scan failed; continuing");
            }
        }

        loop {
            let Some(execution) = self.next_ready_execution() else {
                break;
            };
            let preferred_workspace_id = execution.preferred_workspace_id.clone();
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
                    "worker pool exhausted; deferring dispatch until a worker is released"
                );
                // Invariant: every `tasks.status = 'active'` chore
                // should be backed by a `running` execution / live
                // worker. If the pool stalled with active chores that
                // have no running execution, surface the gap so the
                // ghost-active state isn't silent — the human can
                // compare against `bossctl agents list`.
                if let Ok(orphans) = self.work_db.list_active_chores_without_live_run() {
                    if !orphans.is_empty() {
                        tracing::warn!(
                            ghost_active = ?orphans,
                            pool_capacity,
                            "active chores without a running execution after pool exhaustion \
                             — `boss chore list --status active` and `bossctl agents list` will \
                             diverge until a slot frees up"
                        );
                    }
                }
                break;
            };

            if let Err(err) = self.schedule_execution(&execution, &worker_id).await {
                tracing::error!(
                    ?err,
                    execution_id = %execution.id,
                    worker_id = %worker_id,
                    "failed to start execution"
                );
                self.worker_pool
                    .release_worker(&worker_id, preferred_workspace_id.as_deref())
                    .await;
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
        let work_item = self
            .work_db
            .get_work_item(&execution.work_item_id)
            .with_context(|| format!("failed to resolve work item {}", execution.work_item_id))?;
        let task = execution_task_summary(execution, &work_item);

        let repo = match self
            .cube_client
            .ensure_repo(&execution.repo_remote_url)
            .await
        {
            Ok(repo) => repo,
            Err(err) => {
                self.record_start_failure(execution, worker_id, None, &err)?;
                return Err(err);
            }
        };

        let lease = match self
            .cube_client
            .lease_workspace(
                &repo.repo_id,
                &task,
                execution.preferred_workspace_id.as_deref(),
            )
            .await
        {
            Ok(lease) => lease,
            Err(err) => {
                // Cube returns every lease failure (stale-recovery
                // exhausted, repo missing, db lock, fs error,
                // network, …) as a single anyhow chain whose
                // message embeds the verbatim cube stderr. Log the
                // full chain at error level *before*
                // `record_start_failure` runs, so operators see
                // exactly what cube said instead of just the bare
                // anyhow string that lands on the work record.
                // Stale-working-copy recovery itself is owned by
                // cube (see cube PR #254); the engine treats every
                // cube lease failure as final and does not retry.
                tracing::error!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    worker_id,
                    cube_repo_id = %repo.repo_id,
                    error = format!("{err:#}"),
                    "cube workspace lease failed; marking execution start as failed"
                );
                self.record_start_failure(execution, worker_id, Some(repo.repo_id.as_str()), &err)?;
                return Err(err);
            }
        };
        let change_title = execution_change_title(execution, &work_item);
        let change = match self
            .cube_client
            .create_change(&lease.workspace_path, &change_title)
            .await
        {
            Ok(change) => change,
            Err(err) => {
                if let Err(release_err) = self.cube_client.release_workspace(&lease.lease_id).await
                {
                    tracing::error!(
                        ?release_err,
                        lease_id = %lease.lease_id,
                        "failed to release workspace after change creation failure"
                    );
                }
                self.record_start_failure(execution, worker_id, Some(repo.repo_id.as_str()), &err)?;
                return Err(err);
            }
        };

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
                let coordinator = self.clone();
                tokio::spawn(async move {
                    coordinator
                        .run_execution(execution, run, work_item, worker_id_owned, lease, change)
                        .await;
                });
                Ok(())
            }
            Err(err) => {
                let release_result = self.cube_client.release_workspace(&lease.lease_id).await;
                if let Err(release_err) = release_result {
                    tracing::error!(
                        ?release_err,
                        lease_id = %lease.lease_id,
                        "failed to release workspace after run start failure"
                    );
                }
                Err(err)
            }
        }
    }

    fn record_start_failure(
        &self,
        execution: &WorkExecution,
        worker_id: &str,
        cube_repo_id: Option<&str>,
        error: &anyhow::Error,
    ) -> Result<()> {
        let (execution, run) = self.work_db.fail_execution_start(
            &execution.id,
            worker_id,
            cube_repo_id,
            &error.to_string(),
        )?;
        tracing::warn!(
            execution_id = %execution.id,
            run_id = %run.id,
            worker_id,
            error = %error,
            "recorded execution start failure"
        );
        let publisher = self.publisher.clone();
        let execution_id = execution.id.clone();
        let work_item_id = execution.work_item_id.clone();
        let status = execution.status.clone();
        let product_id = match self.work_db.get_work_item(&work_item_id) {
            Ok(item) => Some(work_item_product_id(&item)),
            Err(err) => {
                tracing::warn!(?err, %work_item_id, "failed to resolve product for runtime broadcast");
                None
            }
        };
        tokio::spawn(async move {
            publisher
                .publish(&execution_id, &work_item_id, &status, "execution_start_failed")
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
        let run_outcome = self
            .execution_runner
            .run_execution(
                &worker_id,
                &execution,
                &work_item,
                lease.workspace_path.as_path(),
                Some(change.change_id.as_str()),
            )
            .await;

        match run_outcome {
            Ok(outcome) => {
                // If the runner allocated a real pane slot for this
                // run (the PaneSpawnRunner case), stamp it onto the
                // run record's agent_id so `bossctl agents list` and
                // related views show one entry per active pane. Pure
                // in-process runners (e.g., AcpExecutionRunner) leave
                // slot_id as None and the worker-pool placeholder
                // (worker_id) stays as the agent_id.
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
            }
            Err(err) => {
                let released = match self.cube_client.release_workspace(&lease.lease_id).await {
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

                match self.work_db.finish_execution_run(
                    &execution.id,
                    &run.id,
                    "failed",
                    "failed",
                    None,
                    Some(error_text.as_str()),
                    released,
                    None,
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

        self.worker_pool
            .release_worker(&worker_id, Some(lease.workspace_id.as_str()))
            .await;
        self.kick();
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
            match self.cube_client.release_workspace(&lease.lease_id).await {
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
            execution_id: execution.id.clone(),
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

struct SchedulingGuard<'a> {
    active: &'a AtomicBool,
}

impl Drop for SchedulingGuard<'_> {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
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

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use tempfile::tempdir;
    use tokio::sync::Mutex;
    use tokio::time::sleep;

    use super::{
        CubeChangeHandle, CubeClient, CubeRepoHandle, CubeWorkspaceLease, CubeWorkspaceStatus,
        ExecutionCoordinator, ExecutionPublisher, MAX_WORKER_POOL_SIZE, WorkerPool,
    };
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
        fail_ensure: bool,
        fail_lease: bool,
        fail_create: bool,
        next_workspace_id: Mutex<Option<String>>,
    }

    impl FakeCubeClient {
        fn with_next_workspace_id(self, id: impl Into<String>) -> Self {
            *self.next_workspace_id.try_lock().expect("uncontended") = Some(id.into());
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
            self.lease_calls.lock().await.push((
                repo_id.to_owned(),
                task.to_owned(),
                prefer_workspace_id.map(str::to_owned),
            ));
            if self.fail_lease {
                return Err(anyhow!("cube workspace lease failed"));
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
    }

    impl Default for FakeExecutionRunner {
        fn default() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail: false,
                pending: false,
                slot_id: None,
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
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
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
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
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
    async fn missing_slot_id_leaves_worker_pool_placeholder_in_agent_id() {
        // Runners without a pane (e.g., AcpExecutionRunner) leave
        // slot_id = None. The coordinator must not touch agent_id in
        // that case — the worker-pool placeholder set at run-create
        // time stays.
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
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
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
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
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_lease: true,
            ..FakeCubeClient::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        ));
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
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_lease: true,
            ..FakeCubeClient::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        ));
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
                    && line.contains(
                        "cube workspace lease failed; marking execution start as failed",
                    )
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

    #[tokio::test]
    async fn change_creation_failure_marks_execution_failed_and_releases_workspace() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        let cube = Arc::new(FakeCubeClient {
            fail_create: true,
            ..FakeCubeClient::default()
        });
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        ));
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
            })
            .unwrap();
        let early = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Old".to_owned(),
                description: None,
                autostart: true,
            })
            .unwrap();
        let late = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "New".to_owned(),
                description: None,
                autostart: true,
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
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
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
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
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
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
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
            })
            .unwrap();
        let first_project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Design A".to_owned(),
                description: None,
                goal: None,
            })
            .unwrap();
        let second_project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Design B".to_owned(),
                description: None,
                goal: None,
            })
            .unwrap();
        db.create_task(CreateTaskInput {
            product_id: product.id.clone(),
            project_id: first_project.id.clone(),
            name: "A1".to_owned(),
            description: None,
            autostart: true,
        })
        .unwrap();
        db.create_task(CreateTaskInput {
            product_id: product.id.clone(),
            project_id: second_project.id.clone(),
            name: "B1".to_owned(),
            description: None,
            autostart: true,
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
            1
        );
        assert_eq!(
            executions
                .iter()
                .filter(|execution| execution.status == "ready")
                .count(),
            3
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
            })
            .unwrap();

        // Ghost A: dragged to Doing but no execution exists at all.
        let ghost_a = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Ghost A".to_owned(),
                description: None,
                autostart: false,
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
        let mut healed_sorted = healed.clone();
        healed_sorted.sort();
        let mut expected = vec![ghost_a.id.clone(), ghost_b.id.clone()];
        expected.sort();
        assert_eq!(healed_sorted, expected, "healed only the ghost rows");

        // Demoted ghosts now sit in `todo`.
        for id in &[&ghost_a.id, &ghost_b.id] {
            match db.get_work_item(id).unwrap() {
                WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, "todo"),
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
            })
            .unwrap();
        let busy = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Already running".to_owned(),
                description: None,
                autostart: true,
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

    /// End-to-end: a chore that's already in the `active` (Doing)
    /// column with a terminal `failed` execution must be picked up
    /// and dispatched the next time the scheduler runs. This is the
    /// "agent freed up while items waited in active" path the
    /// auto-dispatch-on-free-up trigger is meant to drain — the kick
    /// the runtime fires after `release_worker` lands here, and the
    /// new redispatch step inside `run_scheduler` gets the stranded
    /// chore moving without a human nudge.
    #[tokio::test]
    async fn scheduler_redispatches_active_chore_with_terminal_execution() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let stranded = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Stranded".to_owned(),
                description: None,
                autostart: true,
            })
            .unwrap();
        // Stage the "stuck in Doing with a terminal latest execution"
        // shape directly: kanban status `active`, latest exec
        // `failed`. No fresh ready row exists, so the scheduler's
        // ready-queue scan would otherwise leave it untouched.
        db.update_work_item(
            &stranded.id,
            crate::work::WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        db.create_execution(crate::work::CreateExecutionInput {
            work_item_id: stranded.id.clone(),
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
            finished_at: Some("2026-05-07T00:00:00Z".to_owned()),
        })
        .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        ));
        coordinator.kick();

        // Wait for the scheduler to insert a fresh ready exec and run
        // it. Without the free-up redispatch this loop times out
        // because list_ready_executions returns empty forever.
        for _ in 0..200 {
            let executions = db.list_executions(Some(&stranded.id)).unwrap();
            if executions.iter().any(|e| e.status == "waiting_human") {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        let executions = db.list_executions(Some(&stranded.id)).unwrap();
        assert_eq!(
            executions.len(),
            2,
            "expected the original failed exec plus a fresh dispatched one, got {executions:#?}",
        );
        let latest = executions.last().unwrap();
        assert_eq!(
            latest.status, "waiting_human",
            "fresh execution should have been dispatched and run to waiting_human",
        );
    }

    /// Negative case: items whose latest execution is non-terminal
    /// (the `running` worker, or a paused `waiting_human` slot) must
    /// not be touched by the free-up redispatch path. Otherwise the
    /// scheduler would race the live worker mid-spawn and end up
    /// with two executions racing for the same item.
    #[tokio::test]
    async fn scheduler_leaves_running_active_items_alone_on_free_up() {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        // Chore that's already running (active + execution=running).
        let live = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Live".to_owned(),
                description: None,
                autostart: true,
            })
            .unwrap();
        db.update_work_item(
            &live.id,
            crate::work::WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let live_exec = db
            .create_execution(crate::work::CreateExecutionInput {
                work_item_id: live.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("running".to_owned()),
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                cube_repo_id: Some("mono".to_owned()),
                cube_lease_id: Some("lease-existing".to_owned()),
                cube_workspace_id: Some("mono-agent-007".to_owned()),
                workspace_path: Some("/tmp/mono-agent-007".to_owned()),
                priority: None,
                preferred_workspace_id: None,
                started_at: Some("2026-05-07T00:00:00Z".to_owned()),
                finished_at: None,
            })
            .unwrap();

        let cube = Arc::new(FakeCubeClient::default());
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        ));
        coordinator.kick();

        // Give the scheduler enough cycles to misbehave if it's
        // going to. There's nothing for it to dispatch here; the
        // running execution should stay exactly as-is.
        sleep(Duration::from_millis(100)).await;

        let executions = db.list_executions(Some(&live.id)).unwrap();
        assert_eq!(
            executions.len(),
            1,
            "free-up redispatch must not double-create when latest exec is non-terminal, got {executions:#?}",
        );
        assert_eq!(executions[0].id, live_exec.id);
        assert_eq!(executions[0].status, "running");
    }

    /// The pool-grow path is hard-capped at `MAX_WORKER_POOL_SIZE`
    /// because the macOS app only has eight panes. A force-launch
    /// request that arrives with every hard-cap slot busy must surface
    /// a real error instead of silently overcommitting.
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
}
