//! Worker completion detection.
//!
//! `PaneSpawnRunner` returns `WaitingHuman` immediately after spawning
//! the worker pane, so the run row is recorded as `completed` before
//! the worker has actually done any work. The execution sits in
//! `waiting_human` with the cube lease retained, and the linked
//! task/chore stays in `active` (kanban "Doing"). Without something
//! else driving the lifecycle, completed work just sits in Doing
//! forever — that is the bug this module exists to close.
//!
//! The completion signal we listen for is the worker's `Stop` hook
//! event. On every Stop, we look up the workspace path for the run
//! and ask `gh` whether a PR exists for the workspace's current
//! branch. If it does, the work item moves to `in_review`, the
//! execution finalises (status `completed`, lease cleared, finished_at
//! stamped), and the cube workspace is released so the next
//! dispatch can take it over. If there is no PR, we surface an
//! "awaiting input" signal on the execution topic so the
//! coordinator / pane indicator can show the worker is idle without
//! moving the work item to review.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use tokio::process::Command;

use crate::coordinator::{CubeClient, ExecutionPublisher};
use crate::work::{WorkDb, WorkItem};

/// Asks the registered app session to tear down the libghostty pane
/// hosting `run_id`. Implementations must be idempotent: a duplicate
/// call after the slot has been released is a no-op, not an error.
/// The completion handler calls this after a successful cube lease
/// release on PR detection so the Workers grid pane disappears.
#[async_trait]
pub trait WorkerPaneReleaser: Send + Sync {
    async fn release_pane(&self, run_id: &str);
}

/// `WorkerPaneReleaser` that does nothing — used when no app session
/// release is wired (tests, headless runs).
#[derive(Debug, Default)]
pub struct NoopWorkerPaneReleaser;

#[async_trait]
impl WorkerPaneReleaser for NoopWorkerPaneReleaser {
    async fn release_pane(&self, _run_id: &str) {}
}

/// What `gh` reports about the current branch's PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrStatus {
    /// No PR exists for the current branch.
    None,
    /// PR exists and the local branch matches the PR's head commit —
    /// nothing local is unpushed.
    Fresh { url: String },
    /// PR exists, but the local branch is ahead of the PR's pushed
    /// head sha. The PR is stale until the worker pushes; treat as
    /// "no PR yet" for completion purposes.
    Stale { url: String, reason: String },
}

impl PrStatus {
    /// PR url, regardless of whether it is fresh or stale.
    pub fn url(&self) -> Option<&str> {
        match self {
            PrStatus::None => None,
            PrStatus::Fresh { url } | PrStatus::Stale { url, .. } => Some(url),
        }
    }
}

/// Probes a workspace for an open PR on its current branch and
/// reports whether it reflects the local commit history.
#[async_trait]
pub trait PrDetector: Send + Sync {
    /// Returns the workspace's PR status, or `Err(_)` only if `gh`
    /// itself failed in a way distinct from "no PR".
    /// Implementations must treat "no PR" as `Ok(PrStatus::None)` to
    /// keep the caller's idle-vs-completed logic clean.
    async fn detect_pr(&self, workspace_path: &Path) -> Result<PrStatus>;
}

/// `PrDetector` that shells out to `gh pr view` plus `git rev-parse`.
/// The CLI's "no PR for branch" exit is treated as `Ok(PrStatus::None)`;
/// any other non-success exit is propagated as an error so the caller
/// can log it. When a PR exists, the local `HEAD` sha is compared
/// against the PR's `headRefOid`; a mismatch yields
/// `PrStatus::Stale` so the engine can re-probe the worker to push
/// instead of marking the run complete with a stale PR.
#[derive(Debug, Default)]
pub struct CommandPrDetector;

impl CommandPrDetector {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PrDetector for CommandPrDetector {
    async fn detect_pr(&self, workspace_path: &Path) -> Result<PrStatus> {
        // Single `gh pr view` call gets us both the URL and the PR's
        // head sha so we can compare against the local rev without a
        // second round-trip.
        let output = Command::new("gh")
            .args([
                "pr", "view", "--json", "url,headRefOid", "--jq",
                "[.url, .headRefOid] | @tsv",
            ])
            .current_dir(workspace_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| {
                format!(
                    "failed to spawn `gh pr view` in {}",
                    workspace_path.display()
                )
            })?;

        if !output.status.success() {
            // `gh` exits non-zero when there is no PR for the current
            // branch — that is the dominant case and must surface as
            // `Ok(PrStatus::None)`, not an error. Heuristic: stderr
            // mentions "no pull requests" or "no open pull requests".
            let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
            if stderr.contains("no pull requests")
                || stderr.contains("no open pull requests")
                || stderr.contains("no pr found")
            {
                return Ok(PrStatus::None);
            }
            return Err(anyhow!(
                "`gh pr view` failed in {}: {}",
                workspace_path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return Ok(PrStatus::None);
        }

        // `@tsv` joins fields with `\t` and emits a single line per row.
        let mut parts = trimmed.split('\t');
        let url = parts.next().unwrap_or("").trim().to_owned();
        let pr_head = parts.next().unwrap_or("").trim().to_owned();
        if url.is_empty() {
            return Ok(PrStatus::None);
        }

        // `gh pr view` resolves the PR for the current branch, so
        // `git rev-parse HEAD` gives us the right local rev to compare.
        // We deliberately use `git` directly here even though the
        // workspace is jj-managed: jj keeps a real git ref under the
        // hood (the engine's leases sit on git remotes), and this is
        // a read-only query.
        let local_head = match local_head_sha(workspace_path).await {
            Ok(sha) => sha,
            Err(err) => {
                // Couldn't read the local rev — fall back to "fresh"
                // rather than blocking the worker on a bookkeeping
                // glitch. The detector test boundary keeps the
                // caller's contract (Fresh/Stale/None) clean.
                tracing::debug!(
                    workspace = %workspace_path.display(),
                    ?err,
                    "stale-PR check: could not read local HEAD; assuming fresh",
                );
                return Ok(PrStatus::Fresh { url });
            }
        };

        if pr_head.is_empty() || local_head.eq_ignore_ascii_case(&pr_head) {
            Ok(PrStatus::Fresh { url })
        } else {
            Ok(PrStatus::Stale {
                url,
                reason: format!(
                    "local HEAD {local} is ahead of PR head {pr}",
                    local = short_sha(&local_head),
                    pr = short_sha(&pr_head),
                ),
            })
        }
    }
}

/// Read `git rev-parse HEAD` from `workspace_path`. Errors propagate;
/// the caller decides whether to treat that as "fresh" or surface it.
async fn local_head_sha(workspace_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to spawn `git rev-parse HEAD` in {}",
                workspace_path.display()
            )
        })?;
    if !output.status.success() {
        return Err(anyhow!(
            "`git rev-parse HEAD` failed in {}: {}",
            workspace_path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(8).collect()
}

/// Queues an automatic probe for `run_id`. The shape mirrors
/// `ServerState::queue_probe` but is exposed via a trait so the
/// completion handler can be unit-tested without standing up the full
/// app server. Implementations must be cheap and infallible — probes
/// that can't be delivered are dropped silently at injection time
/// (see `dispatch_probe_on_stop` in `app.rs`).
pub trait ProbeQueuer: Send + Sync {
    /// Push `text` onto the FIFO of probes for `run_id`. The next
    /// `Stop` event for the run pops one and `SendToPane`'s it as if
    /// the human had typed it.
    fn queue_probe(&self, run_id: &str, text: &str);
}

/// `ProbeQueuer` that drops everything — used when the test harness
/// doesn't need to assert on probe wiring.
#[derive(Debug, Default)]
pub struct NoopProbeQueuer;

impl ProbeQueuer for NoopProbeQueuer {
    fn queue_probe(&self, _run_id: &str, _text: &str) {}
}

/// Orchestrates the on-Stop completion flow: detect PR, transition
/// state in the work DB, release the cube lease, publish the right
/// invalidation events. Stateless — keeps the wiring side at the call
/// site (`app.rs`) thin.
pub struct WorkerCompletionHandler {
    work_db: Arc<WorkDb>,
    pr_detector: Arc<dyn PrDetector>,
    cube_client: Arc<dyn CubeClient>,
    publisher: Arc<dyn ExecutionPublisher>,
    pane_releaser: Arc<dyn WorkerPaneReleaser>,
    probe_queuer: Arc<dyn ProbeQueuer>,
}

impl WorkerCompletionHandler {
    pub fn new(
        work_db: Arc<WorkDb>,
        pr_detector: Arc<dyn PrDetector>,
        cube_client: Arc<dyn CubeClient>,
        publisher: Arc<dyn ExecutionPublisher>,
        pane_releaser: Arc<dyn WorkerPaneReleaser>,
        probe_queuer: Arc<dyn ProbeQueuer>,
    ) -> Self {
        Self {
            work_db,
            pr_detector,
            cube_client,
            publisher,
            pane_releaser,
            probe_queuer,
        }
    }

    /// Handle a `Stop` event for `execution_id`. Returns the outcome
    /// classification so callers can log/test what happened.
    pub async fn on_stop(&self, execution_id: &str) -> StopOutcome {
        let execution = match self.work_db.get_execution(execution_id) {
            Ok(execution) => execution,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    ?err,
                    "stop event: execution unknown — likely a non-execution worker run"
                );
                return StopOutcome::UnknownExecution;
            }
        };

        // Already completed/failed/cancelled — nothing more to do.
        if !matches!(execution.status.as_str(), "running" | "waiting_human") {
            return StopOutcome::AlreadyTerminal;
        }

        let workspace_path = match execution.workspace_path.as_deref() {
            Some(path) => PathBuf::from(path),
            None => {
                tracing::warn!(
                    execution_id,
                    "stop event: execution has no workspace_path — cannot detect PR"
                );
                return StopOutcome::NoWorkspace;
            }
        };

        let pr_status = match self.pr_detector.detect_pr(&workspace_path).await {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    workspace = %workspace_path.display(),
                    ?err,
                    "stop event: PR detection failed; surfacing as awaiting input"
                );
                self.publish_awaiting_input(&execution).await;
                self.probe_queuer
                    .queue_probe(execution_id, PROBE_DETECTOR_FAILURE);
                return StopOutcome::DetectorFailed;
            }
        };

        let pr_url = match pr_status {
            PrStatus::None => {
                tracing::info!(
                    execution_id,
                    workspace = %workspace_path.display(),
                    "stop event: worker idle without a PR — probing to push and open one"
                );
                self.publish_awaiting_pr(&execution).await;
                self.probe_queuer
                    .queue_probe(execution_id, PROBE_NO_PR);
                return StopOutcome::AwaitingInput;
            }
            PrStatus::Stale { url, reason } => {
                tracing::info!(
                    execution_id,
                    workspace = %workspace_path.display(),
                    pr_url = %url,
                    %reason,
                    "stop event: PR exists but local commits are unpushed — probing to push"
                );
                self.publish_awaiting_pr(&execution).await;
                self.probe_queuer
                    .queue_probe(execution_id, PROBE_STALE_PR);
                return StopOutcome::StalePr { pr_url: url, reason };
            }
            PrStatus::Fresh { url } => url,
        };

        let completion = match self.work_db.record_worker_pr_completion(
            execution_id,
            &pr_url,
            None,
        ) {
            Ok(Some(completion)) => completion,
            Ok(None) => {
                // Race: another Stop event finalised the execution
                // between our status check and the DB update.
                return StopOutcome::AlreadyTerminal;
            }
            Err(err) => {
                tracing::error!(
                    execution_id,
                    ?err,
                    "stop event: failed to record PR completion"
                );
                return StopOutcome::DbError;
            }
        };

        if let Some(lease_id) = completion.released_lease_id.as_deref() {
            if let Err(err) = self.cube_client.release_workspace(lease_id).await {
                tracing::error!(
                    execution_id,
                    lease_id,
                    ?err,
                    "stop event: PR completion recorded but cube release failed"
                );
            }
        }

        // Tear down the libghostty pane that was hosting the worker.
        // Idempotent on the registry side, so a later manual stop /
        // chore-done update for the same run is a no-op.
        self.pane_releaser.release_pane(execution_id).await;

        let product_id = work_item_product_id(&completion.work_item);
        let work_item_id = work_item_id(&completion.work_item);
        self.publisher
            .publish(
                &completion.execution.id,
                &completion.execution.work_item_id,
                &completion.execution.status,
                "worker_pr_completed",
            )
            .await;
        self.publisher
            .publish_work_item_changed(&product_id, &work_item_id, "worker_pr_completed")
            .await;
        tracing::info!(
            execution_id,
            work_item_id = %work_item_id,
            pr_url = %pr_url,
            "stop event: worker PR detected; moved work item to in_review"
        );

        StopOutcome::PrDetected { pr_url }
    }

    /// Force-release the resources backing `execution_id`: tear down
    /// the libghostty pane and release the cube workspace. Idempotent —
    /// duplicate calls (e.g. completion-detection followed by a manual
    /// stop, or two clients racing to mark a chore done) become no-ops
    /// on the second pass via the registry's `take_slot_for_run`
    /// invariant and the DB's lease-id ownership transfer.
    ///
    /// Does NOT change the execution's status field. Callers that need
    /// the execution marked `completed` / `failed` should drive that
    /// transition through the appropriate `WorkDb` method.
    pub async fn force_release(&self, execution_id: &str) {
        // Pane release first. Idempotent on the registry side; the
        // implementation logs and skips when no slot is mapped.
        self.pane_releaser.release_pane(execution_id).await;

        // Cube release: claim ownership of the lease id atomically by
        // clearing it from the DB row before calling the cube CLI.
        // A concurrent caller will see `None` and skip.
        let lease_id = match self.work_db.clear_execution_workspace(execution_id) {
            Ok(Some(lease_id)) => lease_id,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "force_release: failed to clear execution workspace columns",
                );
                return;
            }
        };
        if let Err(err) = self.cube_client.release_workspace(&lease_id).await {
            tracing::warn!(
                execution_id,
                lease_id,
                ?err,
                "force_release: cube workspace release failed",
            );
        }
    }

    async fn publish_awaiting_input(&self, execution: &crate::work::WorkExecution) {
        // Status string mirrors what the execution actually is in DB,
        // but the reason is what carries the "awaiting input" signal
        // — frontends can surface that as the idle/awaiting indicator
        // on the worker pane.
        self.publisher
            .publish(
                &execution.id,
                &execution.work_item_id,
                &execution.status,
                "worker_awaiting_input",
            )
            .await;
    }

    /// Publish the more specific "stopped without a PR" signal so the
    /// frontend can paint a distinct activity icon (the live-state
    /// chore picks this up). Falls back to the same status string as
    /// `awaiting_input` because the execution row hasn't moved.
    async fn publish_awaiting_pr(&self, execution: &crate::work::WorkExecution) {
        self.publisher
            .publish(
                &execution.id,
                &execution.work_item_id,
                &execution.status,
                "worker_awaiting_pr",
            )
            .await;
    }
}

/// Probe text dispatched when a worker stops without producing any PR
/// for its branch. Phrased so a worker that already finished the work
/// will simply push and open one, but a worker that's blocked has an
/// out to explain itself rather than churning.
pub const PROBE_NO_PR: &str = "You stopped without producing a PR for this work. \
If the work is complete, push your branch and open the PR with `gh pr create`. \
If you're blocked, explain what you need.";

/// Probe text dispatched when a PR exists but the worker has local
/// commits that haven't been pushed yet — the PR is stale.
pub const PROBE_STALE_PR: &str = "A PR exists for this branch, but your local commits \
are ahead of the PR's head. Push the new commits (`jj git push -b <bookmark>`) \
so the PR reflects your latest work, or explain why the local commits should not \
be pushed.";

/// Probe text dispatched when the PR detector itself errored. We don't
/// know whether a PR exists, so ask the worker to confirm.
pub const PROBE_DETECTOR_FAILURE: &str = "I couldn't determine whether you've opened \
a PR for this branch (the `gh` query failed). If a PR exists, paste its URL on its \
own line. If not, push your branch and open one with `gh pr create`. If you're \
blocked, explain what you need.";

/// What happened during a stop event handler invocation. The runtime
/// only logs this; tests assert on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// Stop arrived for a run id that doesn't map to a known execution
    /// (e.g., test infra, agent runs).
    UnknownExecution,
    /// Execution was already in a terminal status — no transition.
    AlreadyTerminal,
    /// Execution had no workspace_path recorded.
    NoWorkspace,
    /// `gh` failed with a non-"no-PR" error; surfaced as awaiting input.
    DetectorFailed,
    /// No PR yet — worker is idle awaiting input.
    AwaitingInput,
    /// PR detected; work item moved to `in_review` and execution finalised.
    PrDetected { pr_url: String },
    /// PR exists but local commits are ahead of its head sha. The
    /// worker is probed to push the missing commits; the work item
    /// stays in its current state until the next Stop reports a fresh PR.
    StalePr { pr_url: String, reason: String },
    /// Unexpected DB failure while recording completion.
    DbError,
}

fn work_item_id(item: &WorkItem) -> String {
    match item {
        WorkItem::Product(product) => product.id.clone(),
        WorkItem::Project(project) => project.id.clone(),
        WorkItem::Task(task) | WorkItem::Chore(task) => task.id.clone(),
    }
}

fn work_item_product_id(item: &WorkItem) -> String {
    match item {
        WorkItem::Product(product) => product.id.clone(),
        WorkItem::Project(project) => project.product_id.clone(),
        WorkItem::Task(task) | WorkItem::Chore(task) => task.product_id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    use super::*;
    use crate::coordinator::{
        CubeChangeHandle, CubeClient, CubeRepoHandle, CubeWorkspaceLease, CubeWorkspaceStatus,
    };
    use crate::work::{
        CreateChoreInput, CreateExecutionInput, CreateProductInput, WorkDb, WorkItem,
    };

    struct StubPrDetector {
        result: Mutex<Result<PrStatus, String>>,
    }

    impl StubPrDetector {
        fn ok(value: Option<&str>) -> Arc<Self> {
            let status = match value {
                Some(url) => PrStatus::Fresh { url: url.to_owned() },
                None => PrStatus::None,
            };
            Arc::new(Self {
                result: Mutex::new(Ok(status)),
            })
        }

        fn ok_status(status: PrStatus) -> Arc<Self> {
            Arc::new(Self {
                result: Mutex::new(Ok(status)),
            })
        }

        fn err(message: &str) -> Arc<Self> {
            Arc::new(Self {
                result: Mutex::new(Err(message.to_owned())),
            })
        }
    }

    #[async_trait]
    impl PrDetector for StubPrDetector {
        async fn detect_pr(&self, _workspace_path: &Path) -> Result<PrStatus> {
            let guard = self.result.lock().await;
            match &*guard {
                Ok(value) => Ok(value.clone()),
                Err(msg) => Err(anyhow::anyhow!(msg.clone())),
            }
        }
    }

    #[derive(Default)]
    struct RecordingProbeQueuer {
        calls: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl ProbeQueuer for RecordingProbeQueuer {
        fn queue_probe(&self, run_id: &str, text: &str) {
            self.calls
                .lock()
                .expect("RecordingProbeQueuer mutex poisoned")
                .push((run_id.to_owned(), text.to_owned()));
        }
    }

    impl RecordingProbeQueuer {
        fn snapshot(&self) -> Vec<(String, String)> {
            self.calls
                .lock()
                .expect("RecordingProbeQueuer mutex poisoned")
                .clone()
        }
    }

    #[derive(Default)]
    struct StubCubeClient {
        release_calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl CubeClient for StubCubeClient {
        async fn ensure_repo(&self, _origin: &str) -> Result<CubeRepoHandle> {
            unreachable!("not used in completion tests")
        }
        async fn lease_workspace(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
        ) -> Result<CubeWorkspaceLease> {
            unreachable!("not used in completion tests")
        }
        async fn create_change(
            &self,
            _: &PathBuf,
            _: &str,
        ) -> Result<CubeChangeHandle> {
            unreachable!("not used in completion tests")
        }
        async fn release_workspace(&self, lease_id: &str) -> Result<()> {
            self.release_calls.lock().await.push(lease_id.to_owned());
            Ok(())
        }
        async fn workspace_status(&self, _: &Path) -> Result<CubeWorkspaceStatus> {
            unreachable!("not used in completion tests")
        }
        async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> Result<()> {
            Ok(())
        }
        async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
            Ok(())
        }
        async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct RecordingPaneReleaser {
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl WorkerPaneReleaser for RecordingPaneReleaser {
        async fn release_pane(&self, run_id: &str) {
            self.calls.lock().await.push(run_id.to_owned());
        }
    }

    #[derive(Default)]
    struct RecordingPublisher {
        events: Mutex<Vec<(String, String, String, String)>>,
        work_events: Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait]
    impl ExecutionPublisher for RecordingPublisher {
        async fn publish(&self, exec_id: &str, work_item_id: &str, status: &str, reason: &str) {
            self.events.lock().await.push((
                exec_id.to_owned(),
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
            self.work_events.lock().await.push((
                product_id.to_owned(),
                work_item_id.to_owned(),
                reason.to_owned(),
            ));
        }
    }

    /// Build a WorkDb plus a chore in `waiting_human` execution state with
    /// a cube lease attached — this is the state the engine is in once
    /// `PaneSpawnRunner::run_execution` has returned and
    /// `record_run_completion` has run.
    fn fixture(workspace_path: &Path) -> (Arc<WorkDb>, String, String, String) {
        let dir = tempdir().unwrap();
        // Box-leak the dir; tests are short-lived and this avoids
        // returning the TempDir handle.
        let path = dir.path().join("boss.db");
        std::mem::forget(dir);
        let db = Arc::new(WorkDb::open(path).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".into()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Detect worker stop".into(),
                description: None,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".into(),
                status: Some("ready".into()),
                repo_remote_url: None,
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

        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                workspace_path.to_str().unwrap(),
            )
            .unwrap();
        // Mirror PaneSpawnRunner: run is recorded as completed and the
        // execution sits in `waiting_human` with the lease still held.
        let _ = db
            .finish_execution_run(
                &execution.id,
                &run.id,
                "waiting_human",
                "completed",
                Some("spawned worker pane"),
                None,
                false,
                None,
            )
            .unwrap();

        (db, product.id, chore.id, execution.id)
    }

    #[tokio::test]
    async fn pr_detected_moves_work_item_to_in_review_and_releases_lease() {
        let workspace = tempdir().unwrap();
        let (db, product_id, chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok(Some("https://github.com/foo/bar/pull/42"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.on_stop(&execution_id).await;

        assert!(matches!(outcome, StopOutcome::PrDetected { .. }));
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review");
                assert_eq!(t.pr_url.as_deref(), Some("https://github.com/foo/bar/pull/42"));
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, "completed");
        assert!(execution.cube_lease_id.is_none());
        assert!(execution.workspace_path.is_none());
        assert!(execution.finished_at.is_some());
        assert_eq!(
            cube.release_calls.lock().await.as_slice(),
            ["lease-1"],
            "the engine must release the cube lease so the next dispatch can take it",
        );
        let publisher_events = publisher.events.lock().await.clone();
        assert!(
            publisher_events.iter().any(|(_, _, _, reason)| reason == "worker_pr_completed"),
            "expected worker_pr_completed execution event, got {publisher_events:?}",
        );
        let work_events = publisher.work_events.lock().await.clone();
        assert!(
            work_events
                .iter()
                .any(|(p, w, reason)| p == &product_id
                    && w == &chore_id
                    && reason == "worker_pr_completed"),
            "expected work-item invalidation for the chore, got {work_events:?}",
        );
        assert_eq!(
            pane.calls.lock().await.as_slice(),
            [execution_id.as_str()],
            "pane teardown must fire on PR completion so the libghostty slot returns to Free",
        );
        assert!(
            probes.snapshot().is_empty(),
            "fresh-PR completion must NOT queue a probe — the worker is done",
        );
    }

    #[tokio::test]
    async fn pr_absent_publishes_awaiting_pr_and_queues_probe() {
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.on_stop(&execution_id).await;

        assert_eq!(outcome, StopOutcome::AwaitingInput);
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "active", "no PR must NOT move to in_review");
                assert!(t.pr_url.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, "waiting_human");
        assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
        assert!(
            cube.release_calls.lock().await.is_empty(),
            "no PR must NOT release the cube workspace",
        );
        let events = publisher.events.lock().await.clone();
        assert!(
            events.iter().any(|(_, _, _, reason)| reason == "worker_awaiting_pr"),
            "expected worker_awaiting_pr event for the no-PR case, got {events:?}",
        );
        assert!(
            pane.calls.lock().await.is_empty(),
            "no PR must NOT release the pane",
        );
        let queued = probes.snapshot();
        assert_eq!(
            queued.len(),
            1,
            "exactly one probe must be queued when the worker stops without a PR, got {queued:?}",
        );
        assert_eq!(queued[0].0, execution_id);
        assert_eq!(queued[0].1, PROBE_NO_PR);
    }

    #[tokio::test]
    async fn stale_pr_publishes_awaiting_pr_and_queues_push_probe() {
        // PR exists but local commits are ahead of the PR's head sha.
        // The work item must NOT move to in_review, the lease must
        // stay held, and the worker gets probed to push the missing
        // commits so the next Stop sees a fresh PR.
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok_status(PrStatus::Stale {
            url: "https://github.com/foo/bar/pull/42".into(),
            reason: "local HEAD abcd1234 is ahead of PR head 9876fedc".into(),
        });
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.on_stop(&execution_id).await;

        match outcome {
            StopOutcome::StalePr { pr_url, .. } => {
                assert_eq!(pr_url, "https://github.com/foo/bar/pull/42");
            }
            other => panic!("expected StalePr, got {other:?}"),
        }
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(
                    t.status, "active",
                    "stale PR must NOT move the work item to in_review",
                );
                assert!(t.pr_url.is_none(), "stale PR must NOT stamp pr_url yet");
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, "waiting_human");
        assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
        assert!(cube.release_calls.lock().await.is_empty());
        assert!(pane.calls.lock().await.is_empty());

        let events = publisher.events.lock().await.clone();
        assert!(
            events.iter().any(|(_, _, _, reason)| reason == "worker_awaiting_pr"),
            "stale PR must publish worker_awaiting_pr, got {events:?}",
        );
        let queued = probes.snapshot();
        assert_eq!(queued.len(), 1, "expected one probe, got {queued:?}");
        assert_eq!(queued[0].1, PROBE_STALE_PR);
    }

    #[tokio::test]
    async fn detector_failure_is_treated_as_awaiting_input() {
        let workspace = tempdir().unwrap();
        let (db, _, _, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::err("gh broken");
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db,
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.on_stop(&execution_id).await;
        assert_eq!(outcome, StopOutcome::DetectorFailed);
        assert!(cube.release_calls.lock().await.is_empty());
        assert!(pane.calls.lock().await.is_empty());
        let events = publisher.events.lock().await.clone();
        assert!(
            events.iter().any(|(_, _, _, reason)| reason == "worker_awaiting_input"),
            "detector errors must surface as awaiting_input, got {events:?}",
        );
        let queued = probes.snapshot();
        assert_eq!(queued.len(), 1, "detector failures must still probe the worker");
        assert_eq!(queued[0].1, PROBE_DETECTOR_FAILURE);
    }

    #[tokio::test]
    async fn unknown_execution_is_a_noop() {
        let detector = StubPrDetector::ok(Some("https://github.com/x/y/pull/1"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let handler = WorkerCompletionHandler::new(
            db,
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );
        let outcome = handler.on_stop("not-an-execution").await;
        assert_eq!(outcome, StopOutcome::UnknownExecution);
        assert!(cube.release_calls.lock().await.is_empty());
        assert!(pane.calls.lock().await.is_empty());
        assert!(publisher.events.lock().await.is_empty());
        assert!(
            probes.snapshot().is_empty(),
            "unknown executions must NOT queue probes",
        );
    }

    #[tokio::test]
    async fn force_release_releases_pane_and_cube_lease_then_idempotent() {
        let workspace = tempdir().unwrap();
        let (db, _, _, execution_id) = fixture(workspace.path());
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes.clone(),
        );

        handler.force_release(&execution_id).await;

        // First call: pane fired, cube release fired exactly once.
        assert_eq!(pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
        assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
        let execution = db.get_execution(&execution_id).unwrap();
        assert!(execution.cube_lease_id.is_none());
        assert!(execution.workspace_path.is_none());

        // Second call: idempotent — no second cube release. The pane
        // releaser is invoked again here (the registry-level
        // idempotency lives in `WorkerRegistry::take_slot_for_run`),
        // but no extra cube release happens because the lease columns
        // are already cleared.
        handler.force_release(&execution_id).await;
        assert_eq!(
            cube.release_calls.lock().await.len(),
            1,
            "cube release must fire only once across duplicate force_release calls",
        );
    }

    #[tokio::test]
    async fn force_release_no_lease_skips_cube_release() {
        let workspace = tempdir().unwrap();
        let (db, _, _, execution_id) = fixture(workspace.path());
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());

        // Pre-clear the lease so force_release can confirm it skips
        // cube release when there's nothing to release.
        db.clear_execution_workspace(&execution_id).unwrap();

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes,
        );

        handler.force_release(&execution_id).await;
        assert_eq!(pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
        assert!(cube.release_calls.lock().await.is_empty());
    }

    #[tokio::test]
    async fn duplicate_stop_after_pr_detection_is_idempotent() {
        let workspace = tempdir().unwrap();
        let (db, _, chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok(Some("https://github.com/foo/bar/pull/42"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let pane = Arc::new(RecordingPaneReleaser::default());
        let probes = Arc::new(RecordingProbeQueuer::default());
        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
            pane.clone(),
            probes,
        );

        assert!(matches!(
            handler.on_stop(&execution_id).await,
            StopOutcome::PrDetected { .. }
        ));
        // A second Stop event for the same execution must NOT
        // duplicate work — release is called once, work item stays
        // pinned at `in_review`. The pane releaser is invoked again
        // here; production releasers must be idempotent on their own
        // (see `WorkerRegistry::take_slot_for_run`).
        assert_eq!(
            handler.on_stop(&execution_id).await,
            StopOutcome::AlreadyTerminal,
        );
        assert_eq!(cube.release_calls.lock().await.len(), 1);
        let item = db.get_work_item(&chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => assert_eq!(t.status, "in_review"),
            other => panic!("expected chore, got {other:?}"),
        }
    }
}
