//! In-memory store of per-slot [`LiveWorkerState`] values.
//!
//! The events socket consumer feeds this; bossctl reads from it via
//! the frontend RPC; the topic broker re-publishes the full snapshot
//! whenever any slot changes so UI subscribers can push to the kanban
//! Doing icon and the pane titlebar pill in near-real-time.
//!
//! Keyed by slot id (1..=8), not run id — run records finalise
//! quickly after spawn (they model the spawn act, not the worker's
//! life). Two consecutive runs in the same slot reuse the slot key.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

use boss_protocol::{
    LiveWorkerState, SessionStartSource, WorkItemBinding, WorkerActivity, WorkerEvent,
};

/// The model identifier the engine uses when no `SessionStart` hook
/// has yet reported one — this is the model the launcher *asked* for,
/// surfaced so the UI can render the real model name immediately
/// instead of "Claude Unknown".
pub const DEFAULT_LAUNCH_MODEL: &str = "opus";

/// How long a slot must be stuck in `Spawning` with no hook events before
/// [`LiveWorkerStateRegistry::mark_stalled_spawns`] transitions it to
/// `WaitingForInput`. 30 seconds matches the dead-PID grace period and
/// gives a fresh-but-slow worker enough runway while being well below the
/// typical interactive-wait tolerance.
pub const STALLED_SPAWN_THRESHOLD_SECS: i64 = 30;

/// Thread-safe registry of LiveWorkerState entries, keyed by slot id.
#[derive(Default)]
pub struct LiveWorkerStateRegistry {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    by_slot: HashMap<u8, LiveWorkerState>,
    /// Per-slot flag set when a `Notification` hook arrives, cleared on
    /// the next `Stop`. Lets us turn a `Stop` into `WaitingForInput`
    /// rather than `Idle` when claude is paused on a permission
    /// prompt.
    notification_pending: HashMap<u8, bool>,
    /// Epoch-seconds timestamp recorded when `register_spawn` creates a
    /// slot. Used by `mark_stalled_spawns` to detect workers that have
    /// been stuck in `Spawning` without any hook event (the initial
    /// directory-trust prompt fires before `SessionStart`, so the normal
    /// `Notification`→`WaitingForInput` path is never triggered for it).
    spawned_at: HashMap<u8, i64>,
}

impl LiveWorkerStateRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stamp the initial state for a freshly-allocated slot. Activity
    /// is `Spawning` until the first hook arrives. Any prior entry
    /// for this slot is replaced — the previous worker has been
    /// released, so its terminal state isn't useful.
    ///
    /// `binding` is the work-item linkage for the run. Production
    /// dispatch always passes `Some`; in-process tests and any
    /// future direct-launch path that bypasses the work tables may
    /// pass `None`.
    pub fn register_spawn(
        &self,
        slot_id: u8,
        run_id: impl Into<String>,
        model: impl Into<String>,
        shell_pid: i32,
        binding: Option<WorkItemBinding>,
    ) {
        let state = LiveWorkerState::new_spawning(slot_id, run_id, model, shell_pid, binding);
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        guard.by_slot.insert(slot_id, state);
        guard.notification_pending.remove(&slot_id);
        guard.spawned_at.insert(slot_id, current_epoch_secs());
    }

    /// Drop the entry for `slot_id`. Called when the engine releases
    /// a pane (slot is recycled).
    pub fn release_slot(&self, slot_id: u8) {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        guard.by_slot.remove(&slot_id);
        guard.notification_pending.remove(&slot_id);
        guard.spawned_at.remove(&slot_id);
    }

    /// Snapshot of every entry. Used by the frontend RPC handler and
    /// by the topic publisher.
    pub fn snapshot(&self) -> Vec<LiveWorkerState> {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        let mut out: Vec<LiveWorkerState> = guard.by_slot.values().cloned().collect();
        out.sort_by_key(|s| s.slot_id);
        out
    }

    /// Update the shell pid for the slot that owns `run_id`. Returns
    /// the slot id if the entry was found and updated, or `None` if
    /// no live slot matches. Called when the app sends
    /// `UpdateWorkerShellPid` after the libghostty surface initializes.
    pub fn update_shell_pid(&self, run_id: &str, shell_pid: i32) -> Option<u8> {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        for state in guard.by_slot.values_mut() {
            if state.run_id == run_id {
                let slot_id = state.slot_id;
                state.shell_pid = shell_pid;
                return Some(slot_id);
            }
        }
        None
    }

    /// Look up the state for one slot.
    pub fn get(&self, slot_id: u8) -> Option<LiveWorkerState> {
        self.inner
            .lock()
            .expect("registry mutex poisoned")
            .by_slot
            .get(&slot_id)
            .cloned()
    }

    /// Return the `run_id` of the non-terminal slot currently working on
    /// `work_item_id`, or `None` if no such slot exists. Used by the
    /// chore-update notification path to locate the worker that needs to
    /// hear about an in-flight spec change.
    pub fn run_id_for_work_item(&self, work_item_id: &str) -> Option<String> {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        guard
            .by_slot
            .values()
            .find(|state| {
                !is_terminal_activity(state.activity)
                    && state.work_item_id.as_deref() == Some(work_item_id)
            })
            .map(|state| state.run_id.clone())
    }

    /// True iff a live state entry exists for `run_id` whose activity
    /// indicates the worker is still attached to the slot. Used by
    /// `RequestExecution` to detect "the latest execution is
    /// non-terminal on paper but the worker is gone" — that's the
    /// stale-`waiting_human` shape that would otherwise make a
    /// kanban-driven re-dispatch a silent no-op.
    ///
    /// `Terminated` and `Errored` count as **not** live: the slot is
    /// no longer holding the run open. Everything else
    /// (`Spawning`/`Working`/`WaitingForInput`/`Idle`) does.
    pub fn is_run_live(&self, run_id: &str) -> bool {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        guard
            .by_slot
            .values()
            .any(|state| state.run_id == run_id && !is_terminal_activity(state.activity))
    }

    /// Apply a hook event to the state for `slot_id`. Returns `true`
    /// if the entry actually changed, so callers can suppress no-op
    /// topic pushes. Returns `false` if no entry exists for the slot
    /// (event arrived before spawn registered or after release) — the
    /// caller should treat that as a benign drop.
    ///
    /// Source the model from `SessionStart` events: the hook payload
    /// itself does not currently carry the model id, but we can
    /// safely retain whatever launch default was stamped at spawn.
    pub fn apply_event(&self, slot_id: u8, event: &WorkerEvent) -> bool {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let now = current_iso8601();
        // Split-borrow `Inner` so we can mutate `by_slot` and
        // `notification_pending` simultaneously.
        let Inner {
            by_slot,
            notification_pending,
            ..
        } = &mut *guard;
        let Some(state) = by_slot.get_mut(&slot_id) else {
            return false;
        };
        let before = state.clone();

        state.last_event_at = Some(now);

        match event {
            WorkerEvent::SessionStart { source, .. } => {
                // SessionStart with source=resume keeps the existing
                // model + activity (worker is resuming a session, not
                // spawning fresh). For startup, leave the spawning →
                // idle transition for the first Stop; SessionStart on
                // its own only confirms the worker is alive.
                if matches!(source, SessionStartSource::Startup)
                    && state.activity == WorkerActivity::Spawning
                {
                    state.activity = WorkerActivity::Idle;
                }
            }
            WorkerEvent::UserPromptSubmit { .. } => {
                state.activity = WorkerActivity::Working;
                state.current_tool = None;
            }
            WorkerEvent::PreToolUse { tool_name, .. } => {
                state.activity = WorkerActivity::Working;
                state.current_tool = Some(tool_name.clone());
                notification_pending.remove(&slot_id);
            }
            WorkerEvent::PostToolUse { .. } => {
                state.current_tool = None;
                state.last_tool_ended_at = state.last_event_at.clone();
                // Don't flip to Idle here — Stop is the authoritative
                // turn boundary. Worker may chain multiple tools.
                state.activity = WorkerActivity::Working;
            }
            WorkerEvent::Notification { .. } => {
                state.activity = WorkerActivity::WaitingForInput;
                state.current_tool = None;
                notification_pending.insert(slot_id, true);
            }
            WorkerEvent::Stop { .. } => {
                let was_pending = notification_pending
                    .remove(&slot_id)
                    .unwrap_or(false);
                state.current_tool = None;
                state.activity = if was_pending {
                    WorkerActivity::WaitingForInput
                } else {
                    WorkerActivity::Idle
                };
            }
            WorkerEvent::SessionEnd { .. } => {
                state.activity = WorkerActivity::Terminated;
                state.current_tool = None;
                notification_pending.remove(&slot_id);
            }
        }

        before != *state
    }

    /// Replace the live-status string for `slot_id` and stamp
    /// `live_status_at` with the current ISO-8601 timestamp. Returns
    /// `true` iff the entry actually changed — callers gate the
    /// `broadcast_live_worker_states` push on this exactly like
    /// [`Self::apply_event`] does.
    ///
    /// Pass `Some(text)` to set the field and `None` to clear it
    /// (used when a worker has been idle long enough that the prior
    /// summary would be misleading). Clearing also wipes
    /// `live_status_at` so the staleness UI never has a dangling
    /// timestamp.
    ///
    /// Returns `false` if no entry exists for the slot (event
    /// arrived before spawn registered, or after release) — the
    /// caller treats that as a benign drop, mirroring `apply_event`.
    ///
    /// The registry never decides on its own whether the update is
    /// appropriate for the current activity. The trigger fan-in
    /// owns that policy (e.g., don't refresh while `Spawning`,
    /// suppress stale writes after `Idle`); the registry just stores
    /// the value the caller passed.
    pub fn set_live_status(&self, slot_id: u8, status: Option<String>) -> bool {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let Some(state) = guard.by_slot.get_mut(&slot_id) else {
            return false;
        };
        match (&status, &state.live_status) {
            (None, None) => {
                // Already cleared; nothing to broadcast.
                false
            }
            (None, Some(_)) => {
                // Clearing wipes both halves of the pair so the
                // staleness UI never has a dangling timestamp.
                state.live_status = None;
                state.live_status_at = None;
                true
            }
            (Some(_), _) => {
                // Always advance the timestamp on a successful set —
                // the staleness UI keys off it and the broadcast cost
                // (8 slots × < 1 KiB at < 1 Hz aggregate) is the
                // budget the design's Q6 already accepted. The
                // text-equality short-circuit was tempting but would
                // freeze `last_status_at` until the model picked a
                // different phrasing, which is exactly the
                // "no summarizer activity for >5min" stale signal
                // we'd then misfire on.
                state.live_status = status;
                state.live_status_at = Some(current_iso8601());
                true
            }
        }
    }

    /// Mark a slot as errored. Used when the events socket fails to
    /// decode a payload or repeatedly drops connections. Returns
    /// `true` if the entry actually changed.
    pub fn mark_errored(&self, slot_id: u8) -> bool {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let Some(state) = guard.by_slot.get_mut(&slot_id) else {
            return false;
        };
        if state.activity == WorkerActivity::Errored {
            return false;
        }
        state.activity = WorkerActivity::Errored;
        state.current_tool = None;
        state.last_event_at = Some(current_iso8601());
        true
    }

    /// Detect worker slots stuck in `Spawning` with no hook events for
    /// longer than `threshold_secs` seconds and transition them to
    /// `WaitingForInput`.
    ///
    /// The initial directory-trust prompt that Claude Code shows at
    /// session startup (for models that use `--permission-mode auto`)
    /// fires *before* `SessionStart`, so no hook event ever arrives and
    /// the normal `Notification`→`WaitingForInput` path is never
    /// triggered. An unattended headless worker can never answer the
    /// prompt, so the run stalls indefinitely with no UI signal. This
    /// method is the detection path: if `last_event_at` is `None` (no
    /// hook at all) and the slot has been in `Spawning` for more than
    /// `threshold_secs` seconds, the activity is promoted to
    /// `WaitingForInput` so the existing kanban dot and
    /// `WorkerWaitingIndicator` fire.
    ///
    /// Returns the slot IDs that were changed so callers can broadcast
    /// the updated snapshot. Normal-running workers (whose `SessionStart`
    /// hook fires within seconds of spawn) always have `last_event_at`
    /// set before the threshold elapses; this method ignores them.
    pub fn mark_stalled_spawns(&self, now_epoch_secs: i64, threshold_secs: i64) -> Vec<u8> {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let Inner {
            by_slot,
            spawned_at,
            ..
        } = &mut *guard;
        let cutoff = now_epoch_secs.saturating_sub(threshold_secs);
        let mut changed = Vec::new();
        for (slot_id, state) in by_slot.iter_mut() {
            if state.activity != WorkerActivity::Spawning {
                continue;
            }
            if state.last_event_at.is_some() {
                // SessionStart (or any other hook) already fired — the
                // worker is past the startup phase; not our concern.
                continue;
            }
            let Some(&age_secs) = spawned_at.get(slot_id) else {
                continue;
            };
            if age_secs > cutoff {
                // Spawned too recently; give the worker more time.
                continue;
            }
            state.activity = WorkerActivity::WaitingForInput;
            state.last_event_at = Some(iso8601_utc(now_epoch_secs));
            changed.push(*slot_id);
        }
        changed
    }

    /// Override the recorded spawn timestamp for `slot_id`. Only
    /// available in tests — production code always uses the wall-clock
    /// time stamped by `register_spawn`.
    #[cfg(test)]
    pub fn set_spawn_time_for_test(&self, slot_id: u8, epoch_secs: i64) {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        guard.spawned_at.insert(slot_id, epoch_secs);
    }
}

/// True iff `activity` indicates the worker is no longer attached
/// to its slot — `Terminated` because it exited, `Errored` because
/// the events socket gave up on it. The remaining activity values
/// (`Spawning`, `Working`, `WaitingForInput`, `Idle`) all describe
/// a live, slot-holding worker.
fn is_terminal_activity(activity: WorkerActivity) -> bool {
    matches!(
        activity,
        WorkerActivity::Terminated | WorkerActivity::Errored
    )
}

fn current_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn current_iso8601() -> String {
    let now = SystemTime::now();
    let secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_iso8601_utc(secs)
}

/// Format `epoch_secs` as the same fixed-width ISO-8601 UTC string
/// (`YYYY-MM-DDTHH:MM:SSZ`) the registry stamps into `last_event_at`.
/// Because the format is fixed-width, lexicographic string ordering
/// matches chronological ordering — the stale-worker sweep builds a
/// cutoff timestamp with this and compares `last_event_at < cutoff`
/// directly, with no date parsing.
pub fn iso8601_utc(epoch_secs: i64) -> String {
    format_iso8601_utc(epoch_secs)
}

/// Minimal ISO-8601 UTC formatter (`YYYY-MM-DDTHH:MM:SSZ`). Avoids
/// pulling in chrono just to stamp event timestamps.
fn format_iso8601_utc(epoch_secs: i64) -> String {
    // Days since 1970-01-01.
    let days = epoch_secs.div_euclid(86_400);
    let seconds_in_day = epoch_secs.rem_euclid(86_400);
    let hour = seconds_in_day / 3_600;
    let minute = (seconds_in_day % 3_600) / 60;
    let second = seconds_in_day % 60;
    let (year, month, day) = ymd_from_days_since_1970(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    )
}

/// Convert days-since-1970 into (year, month, day). Adapted from the
/// Howard Hinnant date algorithm.
fn ymd_from_days_since_1970(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::StopReason;

    fn pre_tool(tool: &str) -> WorkerEvent {
        WorkerEvent::PreToolUse {
            session_id: "s".into(),
            tool_name: tool.into(),
            tool_input: serde_json::Value::Null,
        }
    }

    fn post_tool(tool: &str) -> WorkerEvent {
        WorkerEvent::PostToolUse {
            session_id: "s".into(),
            tool_name: tool.into(),
            tool_input: serde_json::Value::Null,
            tool_response: serde_json::Value::Null,
        }
    }

    fn stop_event() -> WorkerEvent {
        WorkerEvent::Stop {
            session_id: "s".into(),
            stop_hook_active: false,
            stop_reason: StopReason::Completed,
        }
    }

    #[test]
    fn update_shell_pid_finds_slot_by_run_id() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(3, "run-abc", "claude-opus-4-7", 0, None);
        let slot = reg.update_shell_pid("run-abc", 55555);
        assert_eq!(slot, Some(3));
        let state = reg.get(3).unwrap();
        assert_eq!(state.shell_pid, 55555);
    }

    #[test]
    fn update_shell_pid_returns_none_for_unknown_run_id() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(3, "run-abc", "claude-opus-4-7", 0, None);
        let slot = reg.update_shell_pid("run-xyz", 99999);
        assert_eq!(slot, None);
        let state = reg.get(3).unwrap();
        assert_eq!(state.shell_pid, 0, "unmatched run must not be modified");
    }

    #[test]
    fn register_spawn_creates_entry_with_spawning_activity() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(2, "run-1", "claude-opus-4-7", 12345, None);
        let state = reg.get(2).unwrap();
        assert_eq!(state.slot_id, 2);
        assert_eq!(state.run_id, "run-1");
        assert_eq!(state.model, "claude-opus-4-7");
        assert_eq!(state.shell_pid, 12345);
        assert_eq!(state.activity, WorkerActivity::Spawning);
        assert!(state.work_item_id.is_none());
        assert!(state.work_item_name.is_none());
        assert!(state.execution_id.is_none());
    }

    #[test]
    fn register_spawn_with_binding_records_work_item_fields() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(
            2,
            "exec-1",
            "claude-opus-4-7",
            12345,
            Some(WorkItemBinding {
                work_item_id: "task_18ad1b81532ac910_4".into(),
                work_item_name: "Fix fencer scraping".into(),
                execution_id: "exec-1".into(),
            }),
        );
        let state = reg.get(2).unwrap();
        assert_eq!(
            state.work_item_id.as_deref(),
            Some("task_18ad1b81532ac910_4")
        );
        assert_eq!(state.work_item_name.as_deref(), Some("Fix fencer scraping"));
        assert_eq!(state.execution_id.as_deref(), Some("exec-1"));
    }

    #[test]
    fn release_slot_clears_entry() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        assert!(reg.get(1).is_some());
        reg.release_slot(1);
        assert!(reg.get(1).is_none());
    }

    #[test]
    fn pre_tool_use_marks_working_with_tool_name() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        let changed = reg.apply_event(1, &pre_tool("Bash"));
        assert!(changed);
        let state = reg.get(1).unwrap();
        assert_eq!(state.activity, WorkerActivity::Working);
        assert_eq!(state.current_tool.as_deref(), Some("Bash"));
        assert!(state.last_event_at.is_some());
    }

    #[test]
    fn post_tool_use_clears_current_tool_and_records_end_time() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.apply_event(1, &pre_tool("Bash"));
        reg.apply_event(1, &post_tool("Bash"));
        let state = reg.get(1).unwrap();
        assert!(state.current_tool.is_none());
        assert!(state.last_tool_ended_at.is_some());
        assert_eq!(state.activity, WorkerActivity::Working);
    }

    #[test]
    fn stop_after_tools_transitions_to_idle() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.apply_event(1, &pre_tool("Bash"));
        reg.apply_event(1, &post_tool("Bash"));
        reg.apply_event(1, &stop_event());
        let state = reg.get(1).unwrap();
        assert_eq!(state.activity, WorkerActivity::Idle);
        assert!(state.current_tool.is_none());
    }

    #[test]
    fn notification_then_stop_marks_waiting_for_input() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.apply_event(
            1,
            &WorkerEvent::Notification {
                session_id: "s".into(),
                message: "claude needs permission".into(),
            },
        );
        reg.apply_event(1, &stop_event());
        let state = reg.get(1).unwrap();
        assert_eq!(state.activity, WorkerActivity::WaitingForInput);
    }

    #[test]
    fn pretooluse_after_notification_clears_pending_flag_and_marks_working() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.apply_event(
            1,
            &WorkerEvent::Notification {
                session_id: "s".into(),
                message: "permission".into(),
            },
        );
        reg.apply_event(1, &pre_tool("Edit"));
        reg.apply_event(1, &stop_event());
        let state = reg.get(1).unwrap();
        // Stop without a fresh notification should now be Idle.
        assert_eq!(state.activity, WorkerActivity::Idle);
    }

    #[test]
    fn session_end_marks_terminated() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.apply_event(
            1,
            &WorkerEvent::SessionEnd {
                session_id: "s".into(),
                reason: "exit".into(),
            },
        );
        let state = reg.get(1).unwrap();
        assert_eq!(state.activity, WorkerActivity::Terminated);
    }

    #[test]
    fn session_start_startup_promotes_spawning_to_idle() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.apply_event(
            1,
            &WorkerEvent::SessionStart {
                session_id: "s".into(),
                source: SessionStartSource::Startup,
            },
        );
        let state = reg.get(1).unwrap();
        assert_eq!(state.activity, WorkerActivity::Idle);
    }

    #[test]
    fn apply_event_returns_false_when_slot_not_registered() {
        let reg = LiveWorkerStateRegistry::new();
        let changed = reg.apply_event(7, &stop_event());
        assert!(!changed);
    }

    #[test]
    fn snapshot_returns_entries_sorted_by_slot() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(3, "run-3", "claude-opus-4-7", 0, None);
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 0, None);
        reg.register_spawn(2, "run-2", "claude-opus-4-7", 0, None);
        let states = reg.snapshot();
        assert_eq!(states.len(), 3);
        assert_eq!(states[0].slot_id, 1);
        assert_eq!(states[1].slot_id, 2);
        assert_eq!(states[2].slot_id, 3);
    }

    #[test]
    fn set_live_status_writes_text_and_stamps_timestamp() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        let changed =
            reg.set_live_status(1, Some("running tests after the layout fix".into()));
        assert!(changed);
        let state = reg.get(1).unwrap();
        assert_eq!(
            state.live_status.as_deref(),
            Some("running tests after the layout fix"),
        );
        assert!(state.live_status_at.is_some());
    }

    #[test]
    fn set_live_status_clears_both_fields() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.set_live_status(1, Some("doing a thing".into()));
        let changed = reg.set_live_status(1, None);
        assert!(changed);
        let state = reg.get(1).unwrap();
        assert!(state.live_status.is_none());
        assert!(state.live_status_at.is_none());
    }

    #[test]
    fn set_live_status_returns_false_when_clearing_already_empty_slot() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        let changed = reg.set_live_status(1, None);
        assert!(!changed);
    }

    #[test]
    fn set_live_status_returns_true_on_repeated_set_to_advance_timestamp() {
        // Two consecutive sets with the same text must still return
        // true so the broadcast fires — the staleness UI keys off
        // `live_status_at`, and freezing it on text equality would
        // misfire the "no summarizer activity" warning.
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        let first = reg.set_live_status(1, Some("running tests".into()));
        let second = reg.set_live_status(1, Some("running tests".into()));
        assert!(first);
        assert!(second);
    }

    #[test]
    fn set_live_status_returns_false_when_slot_unknown() {
        let reg = LiveWorkerStateRegistry::new();
        let changed = reg.set_live_status(7, Some("orphan".into()));
        assert!(!changed);
    }

    #[test]
    fn set_live_status_round_trips_through_snapshot() {
        // The snapshot is what the topic publisher serialises, so
        // confirm that a successful `set_live_status` shows up in
        // both the named getter and the snapshot list.
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(2, "run-2", "claude-opus-4-7", 0, None);
        reg.set_live_status(2, Some("editing the redactor".into()));
        let states = reg.snapshot();
        let s = states.iter().find(|s| s.slot_id == 2).unwrap();
        assert_eq!(s.live_status.as_deref(), Some("editing the redactor"));
        assert!(s.live_status_at.is_some());
    }

    #[test]
    fn release_slot_clears_live_status_pair() {
        // Releasing a slot drops the entry whole, so a subsequent
        // re-spawn into the same slot starts with `None`/`None`.
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.set_live_status(1, Some("doing a thing".into()));
        reg.release_slot(1);
        assert!(reg.get(1).is_none());
        reg.register_spawn(1, "run-2", "claude-opus-4-7", 1, None);
        let state = reg.get(1).unwrap();
        assert!(state.live_status.is_none());
        assert!(state.live_status_at.is_none());
    }

    #[test]
    fn mark_errored_transitions_and_returns_changed() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        assert!(reg.mark_errored(1));
        assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Errored);
        // Idempotent.
        assert!(!reg.mark_errored(1));
    }

    #[test]
    fn run_id_for_work_item_finds_live_binding() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(
            3,
            "exec-42",
            "claude-opus-4-7",
            99,
            Some(WorkItemBinding {
                work_item_id: "chore_abc".into(),
                work_item_name: "My chore".into(),
                execution_id: "exec-42".into(),
            }),
        );
        assert_eq!(
            reg.run_id_for_work_item("chore_abc").as_deref(),
            Some("exec-42")
        );
        assert!(reg.run_id_for_work_item("chore_other").is_none());
    }

    #[test]
    fn run_id_for_work_item_ignores_terminal_slots() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(
            1,
            "exec-dead",
            "claude-opus-4-7",
            10,
            Some(WorkItemBinding {
                work_item_id: "chore_xyz".into(),
                work_item_name: "Terminated chore".into(),
                execution_id: "exec-dead".into(),
            }),
        );
        reg.apply_event(
            1,
            &WorkerEvent::SessionEnd {
                session_id: "s".into(),
                reason: "exit".into(),
            },
        );
        assert!(reg.run_id_for_work_item("chore_xyz").is_none());
    }

    #[test]
    fn iso8601_format_known_epoch() {
        assert_eq!(format_iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_iso8601_utc(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    // ── mark_stalled_spawns (initial directory-trust prompt detection) ────────

    /// Regression test for the initial-directory-trust-prompt detection path.
    ///
    /// The directory-trust prompt that Claude Code shows at session startup
    /// (for Opus / `--permission-mode auto` workers) fires *before*
    /// `SessionStart`, so no hook ever arrives and the slot stays in `Spawning`
    /// with `last_event_at = None`. `mark_stalled_spawns` must detect this and
    /// flip the slot to `WaitingForInput` so the kanban dot + indicator fire.
    #[test]
    fn stalled_spawn_with_no_events_transitions_to_waiting_for_input() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);

        // Backdate the spawn time so the threshold has elapsed.
        let old_spawn = 1_700_000_000_i64;
        reg.set_spawn_time_for_test(1, old_spawn);

        // No hooks have arrived — last_event_at is None, activity is Spawning.
        let before = reg.get(1).unwrap();
        assert_eq!(before.activity, WorkerActivity::Spawning);
        assert!(before.last_event_at.is_none());

        let now = old_spawn + STALLED_SPAWN_THRESHOLD_SECS + 1;
        let changed = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);

        assert_eq!(changed, vec![1], "slot 1 should be reported as changed");
        let after = reg.get(1).unwrap();
        assert_eq!(after.activity, WorkerActivity::WaitingForInput);
        assert!(
            after.last_event_at.is_some(),
            "last_event_at must be stamped on the stall transition"
        );
    }

    /// A worker that received at least one hook event (even just `SessionStart`)
    /// is NOT considered stalled, even if it is still in `Spawning` state
    /// (which can't happen in practice, but is a meaningful boundary).
    #[test]
    fn spawn_with_events_is_not_marked_stalled() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(2, "run-2", "claude-opus-4-7", 1, None);

        // Fire SessionStart so last_event_at gets set.
        reg.apply_event(
            2,
            &WorkerEvent::SessionStart {
                session_id: "s".into(),
                source: SessionStartSource::Startup,
            },
        );

        // Backdate the spawn time.
        reg.set_spawn_time_for_test(2, 1_700_000_000);

        let now = 1_700_000_000 + STALLED_SPAWN_THRESHOLD_SECS + 100;
        let changed = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);

        assert!(changed.is_empty(), "slot with events must not be flagged");
        let state = reg.get(2).unwrap();
        assert_eq!(state.activity, WorkerActivity::Idle);
    }

    /// A worker that spawned very recently is not yet considered stalled —
    /// it just needs more time to start.
    #[test]
    fn freshly_spawned_worker_not_marked_stalled() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(3, "run-3", "claude-opus-4-7", 1, None);

        // The spawn time is "now", so the threshold has not elapsed.
        let now = 1_700_000_100_i64;
        reg.set_spawn_time_for_test(3, now - STALLED_SPAWN_THRESHOLD_SECS + 5);

        let changed = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);

        assert!(changed.is_empty(), "freshly-spawned worker must not be flagged");
        assert_eq!(
            reg.get(3).unwrap().activity,
            WorkerActivity::Spawning,
            "activity must remain Spawning"
        );
    }

    /// Workers in non-Spawning states are never touched by `mark_stalled_spawns`.
    #[test]
    fn non_spawning_states_not_affected_by_stall_detection() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);

        // Advance to Working via PreToolUse.
        reg.apply_event(1, &pre_tool("Bash"));

        reg.set_spawn_time_for_test(1, 1_700_000_000);

        let now = 1_700_000_000 + STALLED_SPAWN_THRESHOLD_SECS + 100;
        let changed = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);

        assert!(changed.is_empty(), "Working slot must not be flagged");
        assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Working);
    }

    /// `mark_stalled_spawns` is idempotent: once a slot transitions to
    /// `WaitingForInput`, it is no longer in `Spawning` and will not be
    /// transitioned again.
    #[test]
    fn mark_stalled_spawns_is_idempotent() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.set_spawn_time_for_test(1, 1_700_000_000);

        let now = 1_700_000_000 + STALLED_SPAWN_THRESHOLD_SECS + 1;
        let first = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);
        assert_eq!(first, vec![1]);

        let second = reg.mark_stalled_spawns(now + 10, STALLED_SPAWN_THRESHOLD_SECS);
        assert!(second.is_empty(), "should not fire again after first transition");
        assert_eq!(
            reg.get(1).unwrap().activity,
            WorkerActivity::WaitingForInput
        );
    }
}
