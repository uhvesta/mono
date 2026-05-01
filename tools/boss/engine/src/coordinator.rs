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
}

#[derive(Debug, Clone)]
pub struct WorkerPool {
    inner: Arc<Mutex<WorkerPoolInner>>,
}

#[derive(Debug)]
struct WorkerPoolInner {
    workers: Vec<WorkerSlot>,
    /// Monotonic counter; the smallest `last_used_seq` among idle workers
    /// is the LRU pick. Workers that have never run a task carry
    /// `last_used_seq = 0`, which sorts ahead of any used worker.
    next_seq: u64,
}

#[derive(Debug, Clone)]
struct WorkerSlot {
    worker_id: String,
    execution_id: Option<String>,
    last_workspace_id: Option<String>,
    last_used_seq: u64,
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
                last_used_seq: 0,
            })
            .collect();
        Self {
            inner: Arc::new(Mutex::new(WorkerPoolInner {
                workers,
                next_seq: 1,
            })),
        }
    }

    /// Claim an idle worker for `execution_id`. Selection is affinity-first:
    /// if `preferred_workspace_id` is set and an idle worker last ran in
    /// that workspace, that worker is chosen. Otherwise the least-recently-
    /// used idle worker wins (workers that have never run sort first).
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

        let lru_idx = inner
            .workers
            .iter()
            .enumerate()
            .filter(|(_, w)| w.execution_id.is_none())
            .min_by_key(|(idx, w)| (w.last_used_seq, *idx))
            .map(|(idx, _)| idx)?;
        let worker = &mut inner.workers[lru_idx];
        worker.execution_id = Some(execution_id.to_owned());
        Some(worker.worker_id.clone())
    }

    /// Release `worker_id` back to the idle pool. If `last_workspace_id`
    /// is provided we record it as the worker's affinity for future
    /// preferred-workspace claims.
    pub async fn release_worker(&self, worker_id: &str, last_workspace_id: Option<&str>) {
        let mut inner = self.inner.lock().await;
        let seq = inner.next_seq;
        inner.next_seq = inner.next_seq.wrapping_add(1).max(1);
        if let Some(worker) = inner
            .workers
            .iter_mut()
            .find(|worker| worker.worker_id == worker_id)
        {
            worker.execution_id = None;
            worker.last_used_seq = seq;
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
}

#[derive(Default)]
pub struct NoopExecutionPublisher;

#[async_trait]
impl ExecutionPublisher for NoopExecutionPublisher {
    async fn publish(&self, _: &str, _: &str, _: &str, _: &str) {}
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

    async fn run_scheduler(self: Arc<Self>) {
        let _guard = SchedulingGuard {
            active: &self.scheduling_active,
        };

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
        tokio::spawn(async move {
            publisher
                .publish(&execution_id, &work_item_id, &status, "execution_start_failed")
                .await;
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
        Ok(())
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
    }

    struct FakeExecutionRunner {
        calls: Mutex<Vec<(String, String, String, Option<String>)>>,
        fail: bool,
        pending: bool,
    }

    impl Default for FakeExecutionRunner {
        fn default() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail: false,
                pending: false,
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
            })
        }
    }

    #[derive(Default)]
    struct RecordingPublisher {
        events: Mutex<Vec<(String, String, String, String)>>,
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
    async fn worker_pool_prefers_workspace_affinity_over_lru() {
        let pool = WorkerPool::new(2);

        // worker-1 picks up exec-a → records affinity to ws-a
        let first = pool.claim_worker("exec-a", None).await.unwrap();
        assert_eq!(first, "worker-1");
        pool.release_worker(&first, Some("ws-a")).await;

        // worker-2 picks up exec-b → records affinity to ws-b
        let second = pool.claim_worker("exec-b", None).await.unwrap();
        assert_eq!(second, "worker-2");
        pool.release_worker(&second, Some("ws-b")).await;

        // worker-1 was used first, so by LRU it would win. But preferring
        // ws-b should pick worker-2 instead.
        let claimed = pool.claim_worker("exec-c", Some("ws-b")).await.unwrap();
        assert_eq!(claimed, "worker-2");
        pool.release_worker(&claimed, Some("ws-b")).await;

        // Without preference, LRU picks the worker that was idle longest;
        // worker-1 has been idle since the first release, so it wins.
        let lru = pool.claim_worker("exec-d", None).await.unwrap();
        assert_eq!(lru, "worker-1");
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
            })
            .unwrap();
        let late = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "New".to_owned(),
                description: None,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        // Bump the later chore's priority — it should run first despite
        // the older one being in the queue first.
        db.request_execution(RequestExecutionInput {
            work_item_id: late.id.clone(),
            priority: Some(10),
            preferred_workspace_id: None,
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
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();
        db.request_execution(RequestExecutionInput {
            work_item_id: chore.id.clone(),
            priority: None,
            preferred_workspace_id: Some("mono-agent-007".to_owned()),
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
        })
        .unwrap();
        db.create_task(CreateTaskInput {
            product_id: product.id.clone(),
            project_id: second_project.id.clone(),
            name: "B1".to_owned(),
            description: None,
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
}
