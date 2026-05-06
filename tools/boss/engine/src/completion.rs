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

/// Probes a workspace for an open PR on its current branch.
#[async_trait]
pub trait PrDetector: Send + Sync {
    /// Returns `Ok(Some(url))` if `gh` reports a PR for the workspace's
    /// current branch, `Ok(None)` if there is no PR yet, and `Err(_)`
    /// only if `gh` itself failed in a way distinct from "no PR".
    /// Implementations must treat "no PR" as `Ok(None)` to keep the
    /// caller's idle-vs-completed logic clean.
    async fn detect_pr(&self, workspace_path: &Path) -> Result<Option<String>>;
}

/// `PrDetector` that shells out to `gh pr view --json url`. The CLI's
/// "no PR for branch" exit is treated as `Ok(None)`; any other
/// non-success exit is propagated as an error so the caller can log it.
#[derive(Debug, Default)]
pub struct CommandPrDetector;

impl CommandPrDetector {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PrDetector for CommandPrDetector {
    async fn detect_pr(&self, workspace_path: &Path) -> Result<Option<String>> {
        let output = Command::new("gh")
            .args(["pr", "view", "--json", "url", "--jq", ".url"])
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

        if output.status.success() {
            let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if url.is_empty() {
                return Ok(None);
            }
            return Ok(Some(url));
        }

        // `gh` exits non-zero when there is no PR for the current
        // branch — that is the dominant case and must surface as
        // `Ok(None)`, not an error. Heuristic: stderr mentions "no
        // pull requests" or "no open pull requests".
        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        if stderr.contains("no pull requests")
            || stderr.contains("no open pull requests")
            || stderr.contains("no pr found")
        {
            return Ok(None);
        }

        Err(anyhow!(
            "`gh pr view` failed in {}: {}",
            workspace_path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
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
}

impl WorkerCompletionHandler {
    pub fn new(
        work_db: Arc<WorkDb>,
        pr_detector: Arc<dyn PrDetector>,
        cube_client: Arc<dyn CubeClient>,
        publisher: Arc<dyn ExecutionPublisher>,
    ) -> Self {
        Self {
            work_db,
            pr_detector,
            cube_client,
            publisher,
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

        let pr_url = match self.pr_detector.detect_pr(&workspace_path).await {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    workspace = %workspace_path.display(),
                    ?err,
                    "stop event: PR detection failed; surfacing as awaiting input"
                );
                self.publish_awaiting_input(&execution).await;
                return StopOutcome::DetectorFailed;
            }
        };

        let Some(pr_url) = pr_url else {
            tracing::info!(
                execution_id,
                workspace = %workspace_path.display(),
                "stop event: worker idle without a PR — awaiting input"
            );
            self.publish_awaiting_input(&execution).await;
            return StopOutcome::AwaitingInput;
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
}

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
        result: Mutex<Result<Option<String>, String>>,
    }

    impl StubPrDetector {
        fn ok(value: Option<&str>) -> Arc<Self> {
            Arc::new(Self {
                result: Mutex::new(Ok(value.map(str::to_owned))),
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
        async fn detect_pr(&self, _workspace_path: &Path) -> Result<Option<String>> {
            let guard = self.result.lock().await;
            match &*guard {
                Ok(value) => Ok(value.clone()),
                Err(msg) => Err(anyhow::anyhow!(msg.clone())),
            }
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

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
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
    }

    #[tokio::test]
    async fn pr_absent_publishes_awaiting_input_and_leaves_state_intact() {
        let workspace = tempdir().unwrap();
        let (db, _product_id, chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok(None);
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
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
            events.iter().any(|(_, _, _, reason)| reason == "worker_awaiting_input"),
            "expected worker_awaiting_input event, got {events:?}",
        );
    }

    #[tokio::test]
    async fn detector_failure_is_treated_as_awaiting_input() {
        let workspace = tempdir().unwrap();
        let (db, _, _, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::err("gh broken");
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());

        let handler = WorkerCompletionHandler::new(db, detector, cube.clone(), publisher.clone());
        let outcome = handler.on_stop(&execution_id).await;
        assert_eq!(outcome, StopOutcome::DetectorFailed);
        assert!(cube.release_calls.lock().await.is_empty());
        let events = publisher.events.lock().await.clone();
        assert!(
            events.iter().any(|(_, _, _, reason)| reason == "worker_awaiting_input"),
            "detector errors must surface as awaiting_input, got {events:?}",
        );
    }

    #[tokio::test]
    async fn unknown_execution_is_a_noop() {
        let detector = StubPrDetector::ok(Some("https://github.com/x/y/pull/1"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let handler =
            WorkerCompletionHandler::new(db, detector, cube.clone(), publisher.clone());
        let outcome = handler.on_stop("not-an-execution").await;
        assert_eq!(outcome, StopOutcome::UnknownExecution);
        assert!(cube.release_calls.lock().await.is_empty());
        assert!(publisher.events.lock().await.is_empty());
    }

    #[tokio::test]
    async fn duplicate_stop_after_pr_detection_is_idempotent() {
        let workspace = tempdir().unwrap();
        let (db, _, chore_id, execution_id) = fixture(workspace.path());
        let detector = StubPrDetector::ok(Some("https://github.com/foo/bar/pull/42"));
        let cube = Arc::new(StubCubeClient::default());
        let publisher = Arc::new(RecordingPublisher::default());
        let handler = WorkerCompletionHandler::new(
            db.clone(),
            detector,
            cube.clone(),
            publisher.clone(),
        );

        assert!(matches!(
            handler.on_stop(&execution_id).await,
            StopOutcome::PrDetected { .. }
        ));
        // A second Stop event for the same execution must NOT
        // duplicate work — release is called once, work item stays
        // pinned at `in_review`.
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
