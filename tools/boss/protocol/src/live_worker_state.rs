//! Live per-slot worker state, derived from hook events delivered to
//! the engine's events socket.
//!
//! [`LiveWorkerState`] is the source of truth for "what is the worker
//! in slot N doing right now". It is keyed by slot rather than by run
//! id because run records finalise quickly after spawn (the spawn act
//! is the run; the worker's *life* is what `LiveWorkerState` models),
//! and because the slot is the durable identifier the UI cares about
//! — a slot persists across the run-record finalisation and is what
//! the kanban Doing icon and the per-pane titlebar pill bind to.
//!
//! The activity values mirror the lifecycle hook events:
//! `Spawning` is the initial state set by the engine spawn flow before
//! any hook has fired; `SessionStart` does not transition activity by
//! itself (it stamps the model name and refreshes timestamps). Once
//! claude is up, activity flip-flops between `Working` (PreToolUse →
//! PostToolUse) and `Idle` (Stop with no pending probe / notification).
//! `WaitingForInput` is set when a `Notification` immediately precedes
//! a `Stop`, indicating claude is paused on a permission prompt.
//! `Errored` and `Terminated` are terminal-ish — `SessionEnd` moves
//! the slot to `Terminated` and the engine's slot allocator clears
//! the entry on release.

use serde::{Deserialize, Serialize};

/// Where a worker is in its life. The engine derives this from hook
/// events arriving on the events socket; UI code maps it to a colour
/// or icon variant. Order is roughly "earlier in the lifecycle" →
/// "later", but the type is not totally ordered — `Idle` and
/// `WaitingForInput` may alternate as the worker runs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerActivity {
    /// Engine has asked the app to allocate a pane and started the
    /// shell, but no `SessionStart` hook has fired yet. The default
    /// state stamped at spawn time.
    Spawning,
    /// Most recent event was `PreToolUse` (without a balancing
    /// `PostToolUse`). The worker is mid-tool-call.
    Working,
    /// `Notification` (or a Stop while a notification was pending) —
    /// claude is awaiting a permission prompt or user redirect. The
    /// kanban Doing icon should signal that the human's attention is
    /// needed.
    WaitingForInput,
    /// `Stop` with no pending probe and no preceding notification.
    /// The worker is between turns, alive but not currently doing
    /// work.
    Idle,
    /// Engine logged an error reading from the worker (malformed hook
    /// payload, repeated socket failure). The slot is still
    /// allocated; the human likely needs to look at logs.
    Errored,
    /// `SessionEnd` fired or the engine released the pane. The entry
    /// is kept around until the slot is reused so callers see the
    /// final state.
    Terminated,
}

impl WorkerActivity {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkerActivity::Spawning => "spawning",
            WorkerActivity::Working => "working",
            WorkerActivity::WaitingForInput => "waiting_for_input",
            WorkerActivity::Idle => "idle",
            WorkerActivity::Errored => "errored",
            WorkerActivity::Terminated => "terminated",
        }
    }
}

/// Identifies the work item a worker slot was dispatched against.
/// Stamped onto [`LiveWorkerState`] at spawn time so the coordinator
/// can resolve "the worker on chore X" without prompting the user
/// for a slot number — see `bossctl agents list` / `agents status`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkItemBinding {
    /// `task_*` / `chore_*` id of the work item powering this run.
    pub work_item_id: String,
    /// Short human-readable name (the work item's `name` column),
    /// useful when the coordinator renders text output.
    pub work_item_name: String,
    /// `work_executions` row id powering this run. The engine
    /// currently uses the same value for `LiveWorkerState.run_id`,
    /// but exposing it under its semantic name keeps callers honest.
    pub execution_id: String,
}

/// Live runtime status for one allocated worker slot. The shape is
/// flat so it serializes cleanly into both the bossctl JSON output
/// and a frontend-socket push.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveWorkerState {
    pub slot_id: u8,
    pub run_id: String,
    /// Model identifier the worker is running on, e.g.
    /// `claude-opus-4-7`. Initially the engine-launched default; once
    /// `SessionStart` reports a model, this updates to the
    /// authoritative value.
    pub model: String,
    /// Best-effort shell pid the app returned at spawn. `0` if the
    /// app did not yet plumb pid back through `proc_listpids`.
    pub shell_pid: i32,
    /// ISO-8601 timestamp of the most recent hook event observed for
    /// this slot. Useful for staleness detection — a worker that has
    /// not emitted any hook in N minutes is likely wedged.
    pub last_event_at: Option<String>,
    /// Tool name in the most recent `PreToolUse` that has not been
    /// balanced by a `PostToolUse`. `None` while the worker is idle.
    pub current_tool: Option<String>,
    /// ISO-8601 timestamp of the most recent `PostToolUse`. Lets
    /// callers compute "tool runtime" or detect a wedged tool.
    pub last_tool_ended_at: Option<String>,
    pub activity: WorkerActivity,
    /// Work item this run was dispatched against. `None` for spawns
    /// that happen outside the work-item dispatch path (today: tests
    /// and any future direct-launch flow that bypasses the work
    /// tables).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_item_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
}

impl LiveWorkerState {
    /// Initial state for a freshly-spawned slot. Activity is
    /// `Spawning`; the model is whatever the engine launched the
    /// worker with (later replaced by the `SessionStart`-reported
    /// value once a hook arrives). `binding` is the work-item
    /// linkage for the run — pass `None` from call sites that don't
    /// have one (tests; future direct-launch).
    pub fn new_spawning(
        slot_id: u8,
        run_id: impl Into<String>,
        model: impl Into<String>,
        shell_pid: i32,
        binding: Option<WorkItemBinding>,
    ) -> Self {
        let (work_item_id, work_item_name, execution_id) = match binding {
            Some(b) => (
                Some(b.work_item_id),
                Some(b.work_item_name),
                Some(b.execution_id),
            ),
            None => (None, None, None),
        };
        Self {
            slot_id,
            run_id: run_id.into(),
            model: model.into(),
            shell_pid,
            last_event_at: None,
            current_tool: None,
            last_tool_ended_at: None,
            activity: WorkerActivity::Spawning,
            work_item_id,
            work_item_name,
            execution_id,
        }
    }
}

/// Topic published when any slot's [`LiveWorkerState`] changes.
/// Subscribers receive the whole snapshot via
/// [`crate::FrontendEvent::WorkerLiveStatesList`].
pub const TOPIC_WORKER_LIVE_STATES: &str = "worker.live_states";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_activity_round_trips_through_serde() {
        for activity in [
            WorkerActivity::Spawning,
            WorkerActivity::Working,
            WorkerActivity::WaitingForInput,
            WorkerActivity::Idle,
            WorkerActivity::Errored,
            WorkerActivity::Terminated,
        ] {
            let json = serde_json::to_string(&activity).unwrap();
            let parsed: WorkerActivity = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, activity);
        }
    }

    #[test]
    fn worker_activity_serializes_as_snake_case() {
        let json = serde_json::to_string(&WorkerActivity::WaitingForInput).unwrap();
        assert_eq!(json, "\"waiting_for_input\"");
    }

    #[test]
    fn new_spawning_sets_defaults() {
        let state = LiveWorkerState::new_spawning(3, "run-1", "claude-opus-4-7", 42, None);
        assert_eq!(state.slot_id, 3);
        assert_eq!(state.run_id, "run-1");
        assert_eq!(state.model, "claude-opus-4-7");
        assert_eq!(state.shell_pid, 42);
        assert_eq!(state.activity, WorkerActivity::Spawning);
        assert!(state.current_tool.is_none());
        assert!(state.last_event_at.is_none());
        assert!(state.last_tool_ended_at.is_none());
        assert!(state.work_item_id.is_none());
        assert!(state.work_item_name.is_none());
        assert!(state.execution_id.is_none());
    }

    #[test]
    fn new_spawning_with_binding_carries_work_item_fields() {
        let state = LiveWorkerState::new_spawning(
            2,
            "exec-9",
            "claude-opus-4-7",
            0,
            Some(WorkItemBinding {
                work_item_id: "task_abc".into(),
                work_item_name: "Fix fencer scraping".into(),
                execution_id: "exec-9".into(),
            }),
        );
        assert_eq!(state.work_item_id.as_deref(), Some("task_abc"));
        assert_eq!(state.work_item_name.as_deref(), Some("Fix fencer scraping"));
        assert_eq!(state.execution_id.as_deref(), Some("exec-9"));
    }

    #[test]
    fn live_worker_state_round_trips() {
        let original = LiveWorkerState {
            slot_id: 1,
            run_id: "run-7".into(),
            model: "claude-sonnet-4-6".into(),
            shell_pid: 12345,
            last_event_at: Some("2026-05-06T12:00:00Z".into()),
            current_tool: Some("Bash".into()),
            last_tool_ended_at: Some("2026-05-06T11:59:50Z".into()),
            activity: WorkerActivity::Working,
            work_item_id: Some("task_42".into()),
            work_item_name: Some("Fix fencer scraping".into()),
            execution_id: Some("run-7".into()),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: LiveWorkerState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn live_worker_state_omits_unbound_fields_when_serializing() {
        let state = LiveWorkerState::new_spawning(1, "exec-1", "claude-opus-4-7", 0, None);
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("work_item_id"), "json: {json}");
        assert!(!json.contains("work_item_name"), "json: {json}");
        assert!(!json.contains("execution_id"), "json: {json}");
    }
}
