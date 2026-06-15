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
    /// The dispatch picked the host this execution will run on (and
    /// built its host adapter). Emitted between `worker_claimed` and
    /// `cube_repo_ensured` to close a silent gap: before this stage
    /// existed, the work-item resolution, host pick, and adapter build
    /// that happen right after a claim produced NO event, so when any
    /// of them failed — `no_eligible_host`, `host_adapter_unavailable`,
    /// or an unresolved work item — the per-execution timeline went
    /// silent after `worker_claimed` and the stall watchdog reaped the
    /// execution ~30s later mislabelling `stalled_stage="worker_claimed"`
    /// (see the automation-pool stall, 2026-06-03). `outcome=ok` carries
    /// the chosen `host_id` in `details`; `outcome=error` carries a
    /// `reason` (`work_item_unresolved` / `no_eligible_host` /
    /// `host_adapter_unavailable`) so a diagnose verb names the real
    /// blocker instead of pointing at the claim.
    HostSelected,
    /// Engine is about to call `cube repo ensure`. Emitted *before* the
    /// subprocess (same rationale as `cube_workspace_lease_attempted`):
    /// `cube repo ensure` on a cold/large repo can run for tens of
    /// seconds, and if it exceeds the `worker_claimed` stall threshold
    /// before returning, the watchdog would otherwise blame the claim.
    /// With this marker the stall is attributed to the repo-ensure
    /// subprocess. `details` carries the origin URL and the timeout.
    CubeRepoEnsureAttempted,
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
    /// The transient-recovery sweep detected a worker that stalled or
    /// died with a *transient* Claude API error as the last entry in
    /// its transcript and auto-resumed it on the same workspace. The
    /// `details` object carries `attempt`, `max_attempts`, the error
    /// `class`, and a clipped `error` string so `bossctl dispatch tail`
    /// shows "recovering, attempt 2/3" without a log dive.
    TransientRecovery,
    /// The transient-recovery sweep stopped retrying a worker and
    /// raised a `WorkAttentionItem` instead — either the error was
    /// non-retryable (permanent / unrecognised) or the retry cap was
    /// reached. The `details` object carries the escalation `reason`.
    TransientRecoveryExhausted,
    /// The transient-recovery sweep sent a runtime nudge to a live idle
    /// worker rather than tearing it down. The worker's `claude` process
    /// is still alive at its REPL and can receive input; a nudge is
    /// cheaper than orphan+respawn. If the nudge does not clear the error
    /// by the next sweep the sweep falls back to the normal
    /// orphan+respawn path.
    TransientRecoveryNudge,
    /// The periodic stale-worker sweep found a slot whose `claude`
    /// process is still alive but has emitted no hook event for longer
    /// than the staleness threshold while `activity=working` with no
    /// tool in flight — the wedged-dependency hang (e.g. a backgrounded
    /// bazel build the worker is idling on that never completes). The
    /// execution has been marked `orphaned`, the pool slot released, and
    /// the work item will be redispatched by the orphan sweep on the
    /// next tick. Distinct from `dead_pid_reconcile` (PID gone) because
    /// here the process is *alive but parked* — `kill(pid, 0)` would
    /// report it healthy.
    StaleWorkerReconcile,
    /// The periodic pool-claim reconciler found a worker-pool slot still
    /// claimed by an execution that is terminal in the DB and has no live
    /// worker pane backing it, and released the claim. This is the
    /// backstop for the leak that wedged the automation pool: every other
    /// slot-releasing path (completion's `release_worker_pane`, the
    /// dead-pid / stale-worker / transient-recovery sweeps) keys off a
    /// live `LiveWorkerStateRegistry` entry, so a claim whose backing
    /// execution terminated WITHOUT a live pane (mid-spawn cancel,
    /// `finalize_pr_transition` DB error, a teardown that dropped the
    /// run→slot mapping but not the pool claim) was released by nothing
    /// and outlived its execution forever. The `details` object carries
    /// the leaked `worker_id`, the terminal `execution_status`, and the
    /// `pool` name so a leak is diagnosable from `bossctl dispatch tail`
    /// without grepping engine logs. Distinct from `dead_pid_reconcile`
    /// (slot has a live-state entry whose PID is gone) — here the slot
    /// has NO live-state entry at all.
    PoolClaimReconcile,
    /// The periodic terminal-work reconciler found a LIVE worker pane whose
    /// bound work item (or its execution) is already terminal — the
    /// O'Brien zombie: a worker that sat alive in `waiting_for_input` for
    /// days after its task went `done` and its PR merged, holding a slot
    /// long after its work was finished. Every other reconciler skips this
    /// case: the dead-pid sweep needs a dead PID *and* a non-terminal
    /// status, the stale-worker sweep only inspects `working` slots, the
    /// transient-recovery sweep recovers unfinished work (never checks
    /// work-item terminality), and the pool-claim sweep deliberately leaves
    /// live-backed claims to the completion path. When that completion
    /// teardown never lands (laptop closed, API call wedged), the worker is
    /// stranded. The `details` object carries the reap `reason`
    /// (`work_item_terminal` / `execution_terminal`), the `slot_id`, and the
    /// terminal `execution_status` so the strand is diagnosable from
    /// `bossctl dispatch tail`. Reaping uses the same idempotent,
    /// run-id-keyed `release_worker_pane` teardown the completion path uses,
    /// so a slot recycled to a different execution between snapshot and reap
    /// is a no-op — an active worker is never reaped.
    TerminalWorkReconcile,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::StatusTransition => "status_transition",
            Stage::RequestRecorded => "request_recorded",
            Stage::WorkerClaimed => "worker_claimed",
            Stage::HostSelected => "host_selected",
            Stage::CubeRepoEnsureAttempted => "cube_repo_ensure_attempted",
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
            Stage::TransientRecovery => "transient_recovery",
            Stage::TransientRecoveryExhausted => "transient_recovery_exhausted",
            Stage::TransientRecoveryNudge => "transient_recovery_nudge",
            Stage::StaleWorkerReconcile => "stale_worker_reconcile",
            Stage::PoolClaimReconcile => "pool_claim_reconcile",
            Stage::TerminalWorkReconcile => "terminal_work_reconcile",
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
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
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
    #[builder(default)]
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
        self.root.join("executions").join(execution_id).join("dispatch.jsonl")
    }

    fn append_line(path: &Path, line: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
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

        let mirror = fs::read_to_string(dir.path().join("executions/exec-a/dispatch.jsonl")).unwrap();
        assert_eq!(mirror.lines().count(), 2);
    }

    #[tokio::test]
    async fn recording_sink_collects_events_per_execution() {
        let sink = RecordingDispatchEventSink::new();
        sink.emit(DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-1"))
            .await;
        sink.emit(DispatchEvent::new(Stage::WorkerClaimed, Outcome::Skipped, "exec-2"))
            .await;
        sink.emit(DispatchEvent::new(Stage::PaneSpawned, Outcome::Error, "exec-1"))
            .await;

        let all = sink.events().await;
        assert_eq!(all.len(), 3);

        let only_one = sink.events_for("exec-1").await;
        assert_eq!(only_one.len(), 2);
        assert_eq!(only_one[0].stage, "request_recorded");
        assert_eq!(only_one[1].stage, "pane_spawned");
        assert_eq!(only_one[1].outcome, "error");
    }

    /// On an `Ok` event the three skip-serialized optionals must be
    /// *absent* from the JSON object (not present-as-null) so the `jq`
    /// expressions downstream tooling uses (`has("error_message")`,
    /// `.cube_command // empty`) behave. `details` still serializes —
    /// as JSON `null` — because it carries no `skip_serializing_if`.
    #[test]
    fn ok_event_omits_skip_serialized_optional_keys() {
        let event = DispatchEvent::new(Stage::RequestRecorded, Outcome::Ok, "exec-omit");
        let value = serde_json::to_value(&event).unwrap();
        let obj = value.as_object().unwrap();

        assert!(
            !obj.contains_key("error_message"),
            "error_message must be omitted on ok"
        );
        assert!(!obj.contains_key("cube_command"), "cube_command must be omitted on ok");
        assert!(!obj.contains_key("cube_cwd"), "cube_cwd must be omitted on ok");

        // details has no skip_serializing_if, so the key stays and is null.
        assert!(obj.contains_key("details"), "details key must always be present");
        assert!(obj["details"].is_null(), "details defaults to JSON null");
    }

    /// Same omission contract holds for a `Skipped` event — the keys
    /// are gated on `Option::is_none`, not on the outcome, but a reader
    /// pinning the skip behaviour on a non-error event must still see
    /// them absent.
    #[test]
    fn skipped_event_omits_skip_serialized_optional_keys() {
        let event = DispatchEvent::new(Stage::WorkerClaimed, Outcome::Skipped, "exec-skip");
        let value = serde_json::to_value(&event).unwrap();
        let obj = value.as_object().unwrap();

        assert!(!obj.contains_key("error_message"));
        assert!(!obj.contains_key("cube_command"));
        assert!(!obj.contains_key("cube_cwd"));
    }

    /// `with_cube_invocation(Some(..))` populates BOTH `cube_command`
    /// and `cube_cwd`, and both survive serialization.
    #[test]
    fn with_cube_invocation_some_sets_both_command_and_cwd() {
        let event = DispatchEvent::new(Stage::CubeWorkspaceLeaseAttempted, Outcome::Ok, "exec-inv")
            .with_cube_invocation(Some((
                "cube workspace lease ci-infra --task \"fix\"".to_owned(),
                "/work/dir".to_owned(),
            )));

        assert_eq!(
            event.cube_command.as_deref(),
            Some("cube workspace lease ci-infra --task \"fix\"")
        );
        assert_eq!(event.cube_cwd.as_deref(), Some("/work/dir"));

        let obj = serde_json::to_value(&event).unwrap();
        assert_eq!(
            obj["cube_command"],
            serde_json::json!("cube workspace lease ci-infra --task \"fix\"")
        );
        assert_eq!(obj["cube_cwd"], serde_json::json!("/work/dir"));
    }

    /// `with_cube_invocation(None)` leaves both fields untouched, so
    /// they stay `None` and are omitted from the JSON object.
    #[test]
    fn with_cube_invocation_none_leaves_both_absent() {
        let event =
            DispatchEvent::new(Stage::CubeWorkspaceLeaseAttempted, Outcome::Ok, "exec-inv").with_cube_invocation(None);

        assert!(event.cube_command.is_none());
        assert!(event.cube_cwd.is_none());

        let obj = serde_json::to_value(&event).unwrap();
        let map = obj.as_object().unwrap();
        assert!(!map.contains_key("cube_command"));
        assert!(!map.contains_key("cube_cwd"));
    }

    /// `with_error` flattens the full anyhow cause chain via `{err:#}`,
    /// so the serialized `error_message` contains the outer context
    /// *and* the root cause, joined by anyhow's `: ` separator.
    #[test]
    fn with_error_flattens_full_anyhow_cause_chain() {
        let err = anyhow::anyhow!("connection refused").context("cube workspace lease failed");
        let event = DispatchEvent::new(Stage::CubeWorkspaceLeaseFailed, Outcome::Error, "exec-err").with_error(&err);

        let obj = serde_json::to_value(&event).unwrap();
        let message = obj["error_message"].as_str().unwrap();

        assert!(
            message.contains("cube workspace lease failed"),
            "outer context missing: {message}"
        );
        assert!(message.contains("connection refused"), "root cause missing: {message}");
        // anyhow's `{:#}` joins each cause with `: `.
        assert_eq!(message, "cube workspace lease failed: connection refused");
    }

    /// The full builder chain populates every optional field and the
    /// values round-trip cleanly through serde back to a `DispatchEvent`
    /// with the expected getters.
    #[test]
    fn full_builder_chain_round_trips_through_serde() {
        let event = DispatchEvent::new(Stage::CubeWorkspaceLeased, Outcome::Ok, "exec-rt")
            .with_work_item("task-rt")
            .with_worker("worker-7")
            .with_cube_repo("repo-9")
            .with_cube_lease("lease-3")
            .with_cube_workspace("ws-2")
            .with_details(serde_json::json!({ "host_id": "host-1", "did_dispatch": true }));

        let line = serde_json::to_string(&event).unwrap();
        let restored: DispatchEvent = serde_json::from_str(&line).unwrap();

        assert_eq!(restored.stage, "cube_workspace_leased");
        assert_eq!(restored.outcome, "ok");
        assert_eq!(restored.execution_id, "exec-rt");
        assert_eq!(restored.work_item_id.as_deref(), Some("task-rt"));
        assert_eq!(restored.worker_id.as_deref(), Some("worker-7"));
        assert_eq!(restored.cube_repo_id.as_deref(), Some("repo-9"));
        assert_eq!(restored.cube_lease_id.as_deref(), Some("lease-3"));
        assert_eq!(restored.cube_workspace_id.as_deref(), Some("ws-2"));
        assert_eq!(restored.details["host_id"], serde_json::json!("host-1"));
        assert_eq!(restored.details["did_dispatch"], serde_json::json!(true));
        assert_eq!(restored.ts_epoch_ms, event.ts_epoch_ms);
    }

    /// A minimal JSON line that omits every skip-serialized optional key
    /// (and `details`) still deserializes — this is the forward/backward
    /// compat guarantee for the wire shape. The absent optionals default
    /// to `None` and `details` defaults to JSON `null`.
    #[test]
    fn deserializes_from_minimal_line_omitting_optional_keys() {
        let line = r#"{
            "ts_epoch_ms": 1700000000000,
            "stage": "request_recorded",
            "outcome": "ok",
            "execution_id": "exec-min"
        }"#;

        let event: DispatchEvent = serde_json::from_str(line).unwrap();

        assert_eq!(event.ts_epoch_ms, 1_700_000_000_000);
        assert_eq!(event.stage, "request_recorded");
        assert_eq!(event.outcome, "ok");
        assert_eq!(event.execution_id, "exec-min");
        assert!(event.work_item_id.is_none());
        assert!(event.worker_id.is_none());
        assert!(event.cube_repo_id.is_none());
        assert!(event.cube_lease_id.is_none());
        assert!(event.cube_workspace_id.is_none());
        assert!(event.error_message.is_none());
        assert!(event.cube_command.is_none());
        assert!(event.cube_cwd.is_none());
        assert!(event.details.is_null());
    }

    /// `Stage::as_str` is the on-disk stage identifier ledger consumers
    /// pin against; a silent rename would break them. Pin every variant
    /// to its exact snake_case string.
    #[test]
    fn stage_as_str_pins_exact_snake_case_identifiers() {
        assert_eq!(Stage::StatusTransition.as_str(), "status_transition");
        assert_eq!(Stage::RequestRecorded.as_str(), "request_recorded");
        assert_eq!(Stage::WorkerClaimed.as_str(), "worker_claimed");
        assert_eq!(Stage::HostSelected.as_str(), "host_selected");
        assert_eq!(Stage::CubeRepoEnsureAttempted.as_str(), "cube_repo_ensure_attempted");
        assert_eq!(Stage::CubeRepoEnsured.as_str(), "cube_repo_ensured");
        assert_eq!(
            Stage::CubeWorkspaceLeaseAttempted.as_str(),
            "cube_workspace_lease_attempted"
        );
        assert_eq!(Stage::CubeWorkspaceLeased.as_str(), "cube_workspace_leased");
        assert_eq!(Stage::CubeWorkspaceLeaseFailed.as_str(), "cube_workspace_lease_failed");
        assert_eq!(Stage::CubeChangeCreated.as_str(), "cube_change_created");
        assert_eq!(Stage::RunStarted.as_str(), "run_started");
        assert_eq!(Stage::PaneSpawned.as_str(), "pane_spawned");
        assert_eq!(Stage::StageStalled.as_str(), "stage_stalled");
        assert_eq!(Stage::OrphanActiveRedispatch.as_str(), "orphan_active_redispatch");
        assert_eq!(Stage::DeadPidReconcile.as_str(), "dead_pid_reconcile");
        assert_eq!(Stage::DispatchDecision.as_str(), "dispatch_decision");
        assert_eq!(Stage::TransientRecovery.as_str(), "transient_recovery");
        assert_eq!(
            Stage::TransientRecoveryExhausted.as_str(),
            "transient_recovery_exhausted"
        );
        assert_eq!(Stage::TransientRecoveryNudge.as_str(), "transient_recovery_nudge");
        assert_eq!(Stage::StaleWorkerReconcile.as_str(), "stale_worker_reconcile");
        assert_eq!(Stage::PoolClaimReconcile.as_str(), "pool_claim_reconcile");
        assert_eq!(Stage::TerminalWorkReconcile.as_str(), "terminal_work_reconcile");
    }

    /// `Outcome::as_str` strings are the on-disk outcome identifiers;
    /// pin them exactly. The serde `rename_all = "snake_case"`
    /// serialization must agree with `as_str`.
    #[test]
    fn outcome_as_str_pins_exact_identifiers() {
        assert_eq!(Outcome::Ok.as_str(), "ok");
        assert_eq!(Outcome::Error.as_str(), "error");
        assert_eq!(Outcome::Skipped.as_str(), "skipped");

        // serde serialization must agree with as_str so the JSON
        // `outcome` field and the in-memory identifier never diverge.
        for outcome in [Outcome::Ok, Outcome::Error, Outcome::Skipped] {
            assert_eq!(
                serde_json::to_value(outcome).unwrap(),
                serde_json::json!(outcome.as_str())
            );
        }
    }
}
