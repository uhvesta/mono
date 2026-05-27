//! Structured, file-backed log of every step in the dispatch
//! pipeline — `RequestExecution` ↦ pane bound to slot — so a silent
//! failure between any two stages can be diagnosed after the fact
//! without re-deriving state.
//!
//! The pipeline is described in detail in
//! [`engine-dispatch-instrumentation.md`]. This module is the
//! minimum production sink that the coordinator and spawn flow can
//! emit into today; downstream phases of that design (CLI verbs,
//! stage-stalled detector, topic broadcast) layer on top.
//!
//! Files live under the existing Boss state root so they survive
//! engine restarts and never share fate with `events.sock` (the
//! engine's *other* stream, which is itself one of the failure
//! modes operators may be diagnosing):
//!
//! ```text
//! boss-state-root/
//!   dispatch-events/
//!     current.jsonl            # source-of-truth flat stream
//!   executions/<execution-id>/
//!     dispatch.jsonl           # mirror of just this execution's lines
//! ```
//!
//! Writers are best-effort: a write that fails to land on disk logs
//! once via `tracing::warn!` and is dropped. Dispatch is never
//! blocked on event emission.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// One step of the dispatch pipeline. Stage values are stable strings
/// so external tooling (`jq`, future bossctl verbs) can pin against
/// them. Spelled provisionally for now — the schema in
/// `engine-dispatch-instrumentation.md` may subsume these names when
/// the full design ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// `UpdateWorkItem` observed a `tasks.status` transition that
    /// would normally trigger auto-dispatch (drag-to-Doing path
    /// from #345). Fires whether or not the dispatch attempt
    /// actually ran — the `details.did_dispatch` flag distinguishes
    /// the two cases. Before this stage existed, a status flip that
    /// fell through the `work_item_needs_dispatch` gate produced no
    /// event at all and the symptom presented as "I dragged it and
    /// nothing happened."
    StatusTransition,
    /// Scheduler picked the execution off the ready queue and is
    /// about to attempt to claim a worker.
    RequestRecorded,
    /// Worker pool returned a free slot (or skipped because every
    /// slot was busy).
    WorkerClaimed,
    /// `cube repo ensure` returned a repo handle.
    CubeRepoEnsured,
    /// Engine is about to call `cube workspace lease`. Emitted *before*
    /// the subprocess invocation so an operator can see what the
    /// engine intended to do (preferred workspace id, fallback
    /// policy) even if the cube call itself hangs and never returns.
    /// The motivating incident hit this exact gap — the engine had
    /// claimed a worker, made the cube call, and then sat silent for
    /// ~46 seconds with no event between `worker_claimed` and the
    /// next stage. Adding an explicit "attempted" record means
    /// `bossctl dispatch diagnose` can show "lease was attempted with
    /// these inputs but the subprocess never came back."
    CubeWorkspaceLeaseAttempted,
    /// `cube workspace lease` returned a lease.
    CubeWorkspaceLeased,
    /// `cube workspace lease` failed (cube returned an error, the
    /// engine timed out the subprocess, or any other reason the
    /// preceding `cube_workspace_lease_attempted` did not progress to
    /// `cube_workspace_leased`). The `error_message` field carries
    /// the verbatim cube stderr / timeout message so a diagnose verb
    /// can render the reason without going back to tracing logs.
    /// Distinct from `cube_workspace_leased` with `outcome=error` so
    /// readers don't have to disambiguate by outcome.
    CubeWorkspaceLeaseFailed,
    /// `cube change create` returned a change handle.
    CubeChangeCreated,
    /// `start_execution_run` committed and `tasks.status` flipped
    /// to `active`.
    RunStarted,
    /// `SpawnWorkerPane` returned ok / error. This is the stage
    /// whose silent failure motivated the structured stream:
    /// before this fix landed, a spawn failure marked the run
    /// `failed` and released the lease without surfacing anything
    /// to the user.
    PaneSpawned,
    /// A non-terminal stage exceeded its per-stage stalled-threshold
    /// without progressing to the next stage. Fires periodically
    /// from the engine's stage-stalled detector; surfaces via
    /// `bossctl dispatch ghost-active --include-stalled`. Does NOT
    /// auto-remediate — the operator decides whether to retry,
    /// reap, or wait.
    StageStalled,
    /// The periodic orphan-active sweep found a work item in `active`
    /// status with no live execution and inserted a fresh `ready`
    /// execution to drive it back into the dispatch pipeline. Distinct
    /// from `status_transition` (which fires on kanban drags) so
    /// `bossctl dispatch tail` can filter orphan-sweep redispatches
    /// separately from human-initiated ones.
    OrphanActiveRedispatch,
    /// The periodic dead-PID sweep found a claimed worker slot whose
    /// backing OS process is gone (ESRCH from `kill(pid, 0)`). The
    /// execution has been marked `orphaned`, the pool slot released,
    /// and the work item will be redispatched by the orphan sweep on
    /// the next tick. Distinct from `orphan_active_redispatch` so
    /// operators can distinguish "slot claimed but PID dead" from
    /// "slot not claimed at all."
    DeadPidReconcile,
    /// A dispatch *trigger* loop (orphan-active sweep, startup
    /// reconcile, worker-release rescan, kanban drag) evaluated whether
    /// a work item needs a fresh dispatch. Emitted UPSTREAM of
    /// `request_recorded` — `request_recorded` only ever fires once the
    /// scheduler has already decided to dispatch, so the decision that
    /// *produced* the request was previously invisible. The `details`
    /// object carries the loop name, the predicate it keyed off, and —
    /// critically — the live execution the loop found (or failed to
    /// account for) so a re-dispatch storm can be traced back to the
    /// loop that re-fired despite a healthy live run. See
    /// `task_18b347260cd7da80_e` (the R693 re-dispatch storm).
    DispatchDecision,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::StatusTransition => "status_transition",
            Stage::RequestRecorded => "request_recorded",
            Stage::WorkerClaimed => "worker_claimed",
            Stage::CubeRepoEnsured => "cube_repo_ensured",
            Stage::CubeWorkspaceLeaseAttempted => "cube_workspace_lease_attempted",
            Stage::CubeWorkspaceLeased => "cube_workspace_leased",
            Stage::CubeWorkspaceLeaseFailed => "cube_workspace_lease_failed",
            Stage::CubeChangeCreated => "cube_change_created",
            Stage::RunStarted => "run_started",
            Stage::PaneSpawned => "pane_spawned",
            Stage::StageStalled => "stage_stalled",
            Stage::OrphanActiveRedispatch => "orphan_active_redispatch",
            Stage::DeadPidReconcile => "dead_pid_reconcile",
            Stage::DispatchDecision => "dispatch_decision",
        }
    }
}

/// Three-valued outcome rather than a boolean so a stage that was
/// reached but decided to skip (e.g., worker pool exhausted) is
/// distinguishable from a stage that errored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Error,
    Skipped,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Error => "error",
            Outcome::Skipped => "skipped",
        }
    }
}

/// One line in the dispatch event stream. The wire shape is
/// deliberately wide — readers don't need to know about every field
/// and a writer that doesn't yet have a value emits `null` rather
/// than dropping the key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchEvent {
    pub ts_epoch_ms: u128,
    pub stage: String,
    pub outcome: String,
    pub execution_id: String,
    pub work_item_id: Option<String>,
    pub worker_id: Option<String>,
    pub cube_repo_id: Option<String>,
    pub cube_lease_id: Option<String>,
    pub cube_workspace_id: Option<String>,
    /// Flat string copy of `format!("{err:#}")` for failure events.
    /// Skip when the outcome is `ok` / `skipped`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Full shell-quoted argv string of the cube subprocess invocation,
    /// e.g. `cube workspace lease ci-infra --task "fix the bug"`.
    /// Copy-pastes into a terminal to reproduce the failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cube_command: Option<String>,
    /// Absolute working directory passed to the cube subprocess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cube_cwd: Option<String>,
    /// Per-stage open object; readers `jq` into this when they care.
    #[serde(default)]
    pub details: serde_json::Value,
}

impl DispatchEvent {
    pub fn new(stage: Stage, outcome: Outcome, execution_id: impl Into<String>) -> Self {
        let ts_epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Self {
            ts_epoch_ms,
            stage: stage.as_str().to_owned(),
            outcome: outcome.as_str().to_owned(),
            execution_id: execution_id.into(),
            work_item_id: None,
            worker_id: None,
            cube_repo_id: None,
            cube_lease_id: None,
            cube_workspace_id: None,
            error_message: None,
            cube_command: None,
            cube_cwd: None,
            details: serde_json::Value::Null,
        }
    }

    pub fn with_work_item(mut self, work_item_id: impl Into<String>) -> Self {
        self.work_item_id = Some(work_item_id.into());
        self
    }

    pub fn with_worker(mut self, worker_id: impl Into<String>) -> Self {
        self.worker_id = Some(worker_id.into());
        self
    }

    pub fn with_cube_repo(mut self, repo_id: impl Into<String>) -> Self {
        self.cube_repo_id = Some(repo_id.into());
        self
    }

    pub fn with_cube_lease(mut self, lease_id: impl Into<String>) -> Self {
        self.cube_lease_id = Some(lease_id.into());
        self
    }

    pub fn with_cube_workspace(mut self, workspace_id: impl Into<String>) -> Self {
        self.cube_workspace_id = Some(workspace_id.into());
        self
    }

    pub fn with_error(mut self, error: &anyhow::Error) -> Self {
        self.error_message = Some(format!("{error:#}"));
        self
    }

    /// Attach `cube_command` and `cube_cwd` from a `(command, cwd)` pair.
    /// Accepts `Option` so callers can pass the result of
    /// `CubeClient::command_repr` directly without an extra `if let`.
    pub fn with_cube_invocation(mut self, info: Option<(String, String)>) -> Self {
        if let Some((cmd, cwd)) = info {
            self.cube_command = Some(cmd);
            self.cube_cwd = Some(cwd);
        }
        self
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }
}

#[async_trait]
pub trait DispatchEventSink: Send + Sync {
    async fn emit(&self, event: DispatchEvent);
}

/// Default sink for tests and any caller that doesn't want the
/// structured stream. Production wiring should use
/// [`JsonlFileSink`] under the Boss state root.
#[derive(Default, Debug, Clone)]
pub struct NoopDispatchEventSink;

#[async_trait]
impl DispatchEventSink for NoopDispatchEventSink {
    async fn emit(&self, _event: DispatchEvent) {}
}

/// Test double: records every event in memory so assertions can
/// inspect the stage timeline without scanning a tracing log.
#[derive(Default, Debug, Clone)]
pub struct RecordingDispatchEventSink {
    events: Arc<Mutex<Vec<DispatchEvent>>>,
}

impl RecordingDispatchEventSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn events(&self) -> Vec<DispatchEvent> {
        self.events.lock().await.clone()
    }

    pub async fn events_for(&self, execution_id: &str) -> Vec<DispatchEvent> {
        self.events
            .lock()
            .await
            .iter()
            .filter(|event| event.execution_id == execution_id)
            .cloned()
            .collect()
    }
}

#[async_trait]
impl DispatchEventSink for RecordingDispatchEventSink {
    async fn emit(&self, event: DispatchEvent) {
        self.events.lock().await.push(event);
    }
}

/// Production sink: appends each event as one JSON line to
/// `<root>/dispatch-events/current.jsonl` and mirrors it into
/// `<root>/executions/<execution_id>/dispatch.jsonl` so a
/// single-execution diagnose verb doesn't need to scan the full
/// stream. Both writes are best-effort; failures log via `tracing`
/// and are dropped.
#[derive(Debug, Clone)]
pub struct JsonlFileSink {
    root: PathBuf,
}

impl JsonlFileSink {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn current_path(&self) -> PathBuf {
        self.root.join("dispatch-events").join("current.jsonl")
    }

    fn execution_path(&self, execution_id: &str) -> PathBuf {
        self.root
            .join("executions")
            .join(execution_id)
            .join("dispatch.jsonl")
    }

    fn append_line(path: &Path, line: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        file.write_all(line)?;
        file.write_all(b"\n")?;
        Ok(())
    }
}

#[async_trait]
impl DispatchEventSink for JsonlFileSink {
    async fn emit(&self, event: DispatchEvent) {
        let serialized = match serde_json::to_vec(&event) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(
                    ?err,
                    stage = %event.stage,
                    execution_id = %event.execution_id,
                    "failed to serialize dispatch event; dropping"
                );
                return;
            }
        };

        let current_path = self.current_path();
        if let Err(err) = Self::append_line(&current_path, &serialized) {
            tracing::warn!(
                ?err,
                path = %current_path.display(),
                stage = %event.stage,
                execution_id = %event.execution_id,
                "failed to append dispatch event to current.jsonl; dropping"
            );
        }

        let execution_path = self.execution_path(&event.execution_id);
        if let Err(err) = Self::append_line(&execution_path, &serialized) {
            tracing::warn!(
                ?err,
                path = %execution_path.display(),
                stage = %event.stage,
                execution_id = %event.execution_id,
                "failed to append dispatch event to per-execution mirror; dropping"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn jsonl_file_sink_appends_to_current_and_mirror() {
        let dir = TempDir::new().unwrap();
        let sink = JsonlFileSink::new(dir.path());

        let event_a = DispatchEvent::new(Stage::CubeWorkspaceLeased, Outcome::Ok, "exec-a")
            .with_work_item("task-a")
            .with_cube_lease("lease-1");
        sink.emit(event_a).await;

        let event_b = DispatchEvent::new(Stage::PaneSpawned, Outcome::Error, "exec-a")
            .with_error(&anyhow::anyhow!("app refused spawn"));
        sink.emit(event_b).await;

        let current = fs::read_to_string(dir.path().join("dispatch-events/current.jsonl")).unwrap();
        let lines: Vec<&str> = current.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("cube_workspace_leased"));
        assert!(lines[1].contains("pane_spawned"));
        assert!(lines[1].contains("app refused spawn"));

        let mirror =
            fs::read_to_string(dir.path().join("executions/exec-a/dispatch.jsonl")).unwrap();
        assert_eq!(mirror.lines().count(), 2);
    }

    #[tokio::test]
    async fn recording_sink_collects_events_per_execution() {
        let sink = RecordingDispatchEventSink::new();
        sink.emit(DispatchEvent::new(
            Stage::RequestRecorded,
            Outcome::Ok,
            "exec-1",
        ))
        .await;
        sink.emit(DispatchEvent::new(
            Stage::WorkerClaimed,
            Outcome::Skipped,
            "exec-2",
        ))
        .await;
        sink.emit(DispatchEvent::new(
            Stage::PaneSpawned,
            Outcome::Error,
            "exec-1",
        ))
        .await;

        let all = sink.events().await;
        assert_eq!(all.len(), 3);

        let only_one = sink.events_for("exec-1").await;
        assert_eq!(only_one.len(), 2);
        assert_eq!(only_one[0].stage, "request_recorded");
        assert_eq!(only_one[1].stage, "pane_spawned");
        assert_eq!(only_one[1].outcome, "error");
    }
}
