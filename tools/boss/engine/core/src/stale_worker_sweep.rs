//! Periodic liveness backstop that detects and reaps worker slots whose
//! `claude` process is still alive but has stopped making progress.
//!
//! ## The hang this guards against
//!
//! A worker can hard-hang without its OS process dying: it backgrounds a
//! pre-push `bazel build`/`bazel test`, then idles in a self-paced loop
//! "until both gates are green". If bazel wedges (host bazel-server
//! contention, `syspolicyd` hang), the status-log files it polls for are
//! never written, the completion notification never arrives, and the
//! worker waits forever. `activity` stays `working`, the PID stays alive,
//! and the worker is indistinguishable from one doing real work. See
//! issue #976 (Crusher / T781, exec `exec_18b4225cf4ee0df0_33d`).
//!
//! [`crate::dead_pid_sweep`] cannot catch this: `kill(pid, 0)` reports
//! the parked `claude` process as perfectly healthy. The distinguishing
//! signal is *transcript progress* — a wedged worker emits no hook
//! events. [`LiveWorkerState::last_event_at`] is stamped on every hook,
//! so "no event for N minutes while `working`" is the liveness gap.
//!
//! ## Algorithm
//!
//! 1. Snapshot [`crate::live_worker_state::LiveWorkerStateRegistry`].
//! 2. For each slot:
//!    1. Skip unless `activity == Working`. `Spawning` is still coming
//!       up; `Idle`/`WaitingForInput` are handled by the completion and
//!       transient-recovery paths; `Terminated`/`Errored` are done.
//!    2. Skip if a tool is in flight (`current_tool.is_some()`). A
//!       *foreground* `bazel build //...` on a cold cache legitimately
//!       runs for many minutes with no intervening hook — reaping it
//!       would be the regression we must not cause. The companion fix
//!       (the pre-push gate's `timeout` guidance) bounds that case; this
//!       sweep deliberately only targets the *idle-between-tools* wedge.
//!    3. Skip if `last_event_at` is newer than the staleness threshold,
//!       or absent (no hook has landed yet).
//!    4. Age guard against the DB `started_at` (skip fresh dispatches).
//! 3. For a confirmed-stale slot: mark the execution `orphaned`, append
//!    an `[engine-reconcile]` audit line, release the pool slot, emit a
//!    `stale_worker_reconcile` dispatch event, and kick the coordinator
//!    so the orphan sweep redispatches the committed-but-stranded work.
//!
//! ## False-positive guards
//!
//! The combination of (a) `activity == Working`, (b) no tool in flight,
//! and (c) a generous [`DEFAULT_STALE_THRESHOLD_SECS`] (30 min) keeps
//! this conservative. A worker that is merely thinking, streaming a long
//! response, or running a foreground command emits a hook (or holds
//! `current_tool`) well inside that window; only a worker that has
//! genuinely parked itself trips the reap. The [`STALE_GRACE_SECS`]
//! guard additionally skips executions whose `started_at` is too recent
//! to have a meaningful event history.
//!
//! ## Cadence
//!
//! Runs every 60 seconds and fires once immediately on boot (same
//! pattern as [`crate::dead_pid_sweep`] / [`crate::orphan_sweep`]).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use boss_protocol::{WorkItemPatch, WorkerActivity};

use crate::coordinator::{ExecutionCoordinator, worker_id_for_slot};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::{LiveWorkerStateRegistry, iso8601_utc};
use crate::work::WorkDb;

/// No hook event for this long while a worker is `working` with no tool
/// in flight ⇒ presumed wedged. 30 minutes is deliberately generous: a
/// healthy worker emits a `PreToolUse`/`PostToolUse`/`UserPromptSubmit`
/// hook far more often than this, so the threshold sits well clear of
/// normal think/stream gaps while still bounding the indefinite hang the
/// incident exhibited (~35 min and counting before manual recovery).
pub const DEFAULT_STALE_THRESHOLD_SECS: i64 = 1_800;

/// Grace period after `started_at` (epoch seconds) during which we skip
/// staleness probing, mirroring [`crate::dead_pid_sweep::DEAD_PID_GRACE_SECS`].
/// Guards against reaping a freshly-dispatched run whose pane is still
/// spinning up and has not yet emitted its first hook.
pub const STALE_GRACE_SECS: i64 = 60;

/// Reaps a confirmed-stale worker's OS process tree and tears down its
/// pane/slot — the exact teardown `bossctl agents stop` performs (the
/// `release_worker_pane` path: app pane release → `reap_worker_process_tree`
/// SIGTERM/SIGKILL ladder → pool-slot release → live-state drop).
///
/// The reconcile path *must* go through this before the cube workspace
/// becomes eligible for re-lease. The original sweep released the pool
/// slot without ever killing the `claude` process, so a redispatch's
/// `any_free` lease could land in the still-occupied workspace and two
/// live workers would interleave edits in one working copy. Freeing the
/// slot while the process lives converts a false-positive cancel into a
/// workspace-sharing catastrophe; reaping first closes that gap (same
/// requirement as the `bossctl agents stop` leak in #1006 and the
/// PR-merge retire path in T1561 — this is yet another retire path that
/// skipped teardown).
#[async_trait::async_trait]
pub trait StaleWorkerReaper: Send + Sync {
    /// Kill the worker process tree for `execution_id` and release its
    /// pane/slot. Idempotent: a worker already gone is a no-op.
    async fn reap_worker(&self, execution_id: &str);
}

/// Counts from one pass of the sweep; logged at `info` when a reap
/// occurs.
#[derive(Debug, Default, bon::Builder)]
#[builder(on(String, into))]
pub struct StaleWorkerSweepOutcome {
    pub reaped: usize,
    pub fresh_skipped: usize,
    pub tool_in_flight_skipped: usize,
    pub not_working_skipped: usize,
    pub grace_skipped: usize,
    /// Slots skipped because the live-state `last_event_at` predates the
    /// execution's own `started_at` — a mis-attributed event from a
    /// recycled slot (the false-positive cancel guard, defect 1).
    pub pre_start_event_skipped: usize,
}

impl StaleWorkerSweepOutcome {
    fn has_activity(&self) -> bool {
        self.reaped > 0
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn so a worker that wedged before the engine
/// restarted is recovered at boot without waiting for the first interval.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    live_states: Arc<LiveWorkerStateRegistry>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    reaper: Arc<dyn StaleWorkerReaper>,
    interval: Duration,
    stale_threshold_secs: i64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let outcome = run_one_pass(
                work_db.as_ref(),
                live_states.as_ref(),
                coordinator.clone(),
                dispatch_events.as_ref(),
                reaper.as_ref(),
                stale_threshold_secs,
            )
            .await;
            if outcome.has_activity() {
                tracing::info!(
                    reaped = outcome.reaped,
                    fresh_skipped = outcome.fresh_skipped,
                    tool_in_flight_skipped = outcome.tool_in_flight_skipped,
                    grace_skipped = outcome.grace_skipped,
                    "stale-worker sweep: pass complete",
                );
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// Run a single stale-worker sweep pass. Returns a summary of what
/// happened; callers may log it.
///
/// Takes `coordinator` as `Arc` because kicking the scheduler requires
/// `Arc<ExecutionCoordinator>` — the kick path spawns a tokio task that
/// holds a reference.
pub async fn run_one_pass(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
    reaper: &dyn StaleWorkerReaper,
    stale_threshold_secs: i64,
) -> StaleWorkerSweepOutcome {
    let mut outcome = StaleWorkerSweepOutcome::default();
    let snapshot = live_states.snapshot();

    let now_epoch_secs: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let grace_cutoff = now_epoch_secs - STALE_GRACE_SECS;
    // Build the staleness cutoff as a fixed-width ISO-8601 string so we
    // can compare `last_event_at < stale_cutoff` lexicographically — the
    // format is the same one the registry stamps, so byte order matches
    // chronological order and no date parsing is needed.
    let stale_cutoff = iso8601_utc(now_epoch_secs - stale_threshold_secs);

    for state in snapshot {
        // Only `working` slots are candidates. `Spawning` is still
        // coming up (no event history expected); `Idle` and
        // `WaitingForInput` are handled by the completion and
        // transient-recovery paths; terminal states are done.
        if state.activity != WorkerActivity::Working {
            outcome.not_working_skipped += 1;
            continue;
        }

        // A tool in flight means the worker is legitimately busy — most
        // importantly a long foreground `bazel build`/`bazel test`,
        // which can run for many minutes with no intervening hook.
        // Reaping that would break real work; skip it and let the
        // pre-push gate's `timeout` guidance bound the wedged-tool case.
        if state.current_tool.is_some() {
            outcome.tool_in_flight_skipped += 1;
            continue;
        }

        // No hook yet at all ⇒ nothing to judge staleness against; the
        // dead-PID / grace paths cover a truly stuck spawn.
        let Some(last_event_at) = state.last_event_at.as_deref() else {
            outcome.fresh_skipped += 1;
            continue;
        };

        // Newer than the threshold ⇒ healthy.
        if last_event_at >= stale_cutoff.as_str() {
            outcome.fresh_skipped += 1;
            continue;
        }

        let execution_id = &state.run_id;

        // Look up the execution for the age guard and work_item_id.
        let execution = match work_db.get_execution(execution_id) {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "stale-worker sweep: failed to look up execution; skipping slot",
                );
                continue;
            }
        };

        // Skip executions already in a terminal DB state (completion
        // path may have raced the sweep).
        if execution.status.is_terminal() {
            continue;
        }

        // Grace-period guard: skip executions whose `started_at` is
        // within STALE_GRACE_SECS or not yet recorded.
        let Some(started_epoch) = execution.started_at.as_deref().and_then(|s| s.parse::<i64>().ok()) else {
            outcome.grace_skipped += 1;
            continue;
        };
        if started_epoch >= grace_cutoff {
            outcome.grace_skipped += 1;
            continue;
        }

        // Event-attribution guard (defect 1 — the false-positive cancel).
        //
        // The `last_event_at` we are about to judge MUST belong to THIS
        // execution. The live-state registry is keyed by *slot*, and a
        // slot is reused across consecutive runs; on a slot recycle the
        // events-socket / live-state association can leave a *prior run's*
        // last-event timestamp attached to the slot (the slot/exec/pane
        // identity class investigated in PR #1213). A hook timestamp that
        // predates the execution's own `started_at` cannot possibly be one
        // of its events — it is that recycled-slot artifact. Reaping on it
        // false-cancels a healthy, actively-working worker, releases its
        // lease, and lets a redispatch's `any_free` lease collide in the
        // same workspace (the incident this fix exists for). Key the
        // staleness decision to the current execution's own timeline:
        // never treat a pre-start timestamp as in-execution activity. A
        // worker whose events are genuinely flowing always stamps
        // `last_event_at` at or after `started_at`, so this can only skip
        // the mis-attributed case — a worker with flowing events is
        // un-cancellable by this path. Log loudly so the misattribution is
        // visible without ever cancelling a live worker.
        let started_iso = iso8601_utc(started_epoch);
        if last_event_at < started_iso.as_str() {
            tracing::warn!(
                execution_id,
                slot_id = state.slot_id,
                last_event_at,
                started_at = %started_iso,
                stale_threshold_secs,
                "stale-worker sweep: last_event_at predates this execution's started_at — \
                 mis-attributed event from a recycled slot (cf. PR #1213); NOT reaping \
                 (worker presumed healthy, staleness un-evaluable for this run)",
            );
            outcome.pre_start_event_skipped += 1;
            continue;
        }

        tracing::info!(
            execution_id,
            work_item_id = %execution.work_item_id,
            slot_id = state.slot_id,
            last_event_at,
            stale_threshold_secs,
            "stale-worker sweep: worker alive but no progress past threshold; reaping execution and releasing slot",
        );

        // Mark the execution orphaned so the DB reflects the wedge and
        // `bossctl agents transcript <exec-id>` still works.
        let reason = format!(
            "stale-worker-reconcile: no hook event since {last_event_at} (> {stale_threshold_secs}s) while working with no tool in flight; worker presumed wedged on a backgrounded/idle wait"
        );
        if let Err(err) = work_db.mark_execution_orphaned(execution_id, &reason) {
            tracing::warn!(
                execution_id,
                ?err,
                "stale-worker sweep: failed to mark execution orphaned; skipping reap",
            );
            continue;
        }

        // Snapshot the wedged worker's uncommitted workspace work to a
        // durable patch before the slot is released and the workspace
        // becomes eligible for re-lease/reset. Best-effort: a failed or
        // empty capture returns None and never blocks the reap.
        let recovery_patch = crate::recovery_backup::backup_dead_execution(&execution);

        // Append [engine-reconcile] audit line to the task description so
        // a human inspecting the chore can see why it was reset (and
        // where to find the recovery patch, if one was captured).
        if let Some(work_item_id) = &state.work_item_id
            && let Err(err) = append_reconcile_audit(
                work_db,
                work_item_id,
                execution_id,
                now_epoch_secs,
                stale_threshold_secs,
                recovery_patch.as_deref(),
            )
        {
            tracing::warn!(
                work_item_id,
                ?err,
                "stale-worker sweep: failed to append audit line to description (non-fatal)",
            );
        }

        // Reap the worker's OS process tree BEFORE the slot/lease is
        // freed (defect 2). The original sweep released the pool slot
        // without ever killing the `claude` process — so a redispatch's
        // `any_free` lease could land in the still-occupied workspace and
        // two live workers would interleave edits in one working copy.
        // Route through the same teardown `bossctl agents stop` uses
        // (`release_worker_pane`: app pane release → process-tree
        // SIGTERM/SIGKILL → pool-slot release → live-state drop), so the
        // process is dead (at minimum SIGTERM-signalled) before the kick
        // below can trigger a redispatch that re-leases the workspace.
        // This must precede any lease release; a lease freed while the
        // process lives is what turned the false cancel into a
        // workspace-sharing catastrophe.
        reaper.reap_worker(execution_id).await;

        // Release the worker pool slot so the orphan sweep detects the
        // chore and creates a fresh ready execution for redispatch.
        // Use worker_id_for_slot (not WorkerPool::worker_id_for_slot) so
        // automation-pool slots (> MAX_WORKER_POOL_SIZE) produce the
        // "auto-worker-N" prefix and release_worker_and_kick routes to the
        // correct pool via pool_for_worker_id. Idempotent with the
        // pool-slot release the reaper's `release_worker_pane` already
        // performed in production (find-or-skip no-op); in tests where the
        // reaper is a recording stub, this is what frees the slot.
        let worker_id = worker_id_for_slot(state.slot_id);
        coordinator.release_worker_and_kick(&worker_id, None).await;

        // Structured event for bossctl dispatch tail.
        dispatch_events
            .emit(
                DispatchEvent::new(Stage::StaleWorkerReconcile, Outcome::Ok, execution_id)
                    .with_work_item(&execution.work_item_id)
                    .with_details(serde_json::json!({
                        "slot_id": state.slot_id,
                        "last_event_at": last_event_at,
                        "stale_threshold_secs": stale_threshold_secs,
                        "recovery_patch": recovery_patch
                            .as_deref()
                            .map(|p| p.display().to_string()),
                    })),
            )
            .await;

        outcome.reaped += 1;
    }

    outcome
}

/// Append an `[engine-reconcile]` audit line to the work item's
/// description so an operator can see why the chore was reset.
fn append_reconcile_audit(
    work_db: &WorkDb,
    work_item_id: &str,
    stale_execution_id: &str,
    now_epoch_secs: i64,
    stale_threshold_secs: i64,
    recovery_patch: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let item = work_db.get_work_item(work_item_id)?;
    let current_desc = match &item {
        boss_protocol::WorkItem::Product(p) => p.description.as_str(),
        boss_protocol::WorkItem::Project(p) => p.description.as_str(),
        boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => t.description.as_str(),
    };
    let recovery_note = match recovery_patch {
        Some(path) => format!(" Uncommitted work backed up to {}.", path.display()),
        None => String::new(),
    };
    let audit_line = format!(
        "\n[engine-reconcile] epoch {now_epoch_secs}: stale worker (exec {stale_execution_id}) detected — no transcript progress for > {stale_threshold_secs}s while working; chore reset to todo for redispatch.{recovery_note}"
    );
    let new_desc = format!("{current_desc}{audit_line}");
    work_db.update_work_item(
        work_item_id,
        WorkItemPatch {
            description: Some(new_desc),
            ..WorkItemPatch::default()
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;
    use async_trait::async_trait;
    use boss_protocol::{WorkItemBinding, WorkerEvent};
    use tempfile::TempDir;

    use super::*;
    use crate::coordinator::{
        CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
        ExecutionCoordinator, WorkerPool,
    };
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::live_worker_state::LiveWorkerStateRegistry;
    use crate::runner::{ExecutionRunner, RunOutcome};
    use crate::work::{CreateChoreInput, CreateProductInput, ExecutionStatus, WorkDb, WorkItemPatch};
    use boss_protocol::WorkExecution;

    // A staleness threshold whose cutoff lands in the *future*, so any
    // `last_event_at` stamped "now" by `apply_event` compares as stale.
    // This lets us exercise the staleness branch deterministically
    // without a way to backdate the in-memory `last_event_at`.
    const ALWAYS_STALE: i64 = -120;
    // A threshold whose cutoff is an hour in the past, so a just-stamped
    // event is comfortably fresh.
    const NEVER_STALE: i64 = 3_600;

    // ─── stubs (mirrors dead_pid_sweep) ──────────────────────────────────────

    struct NoopCube;

    #[async_trait]
    impl CubeClient for NoopCube {
        async fn ensure_repo(&self, _: &str) -> Result<CubeRepoHandle> {
            unimplemented!()
        }
        async fn lease_workspace(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: bool,
            _: &[&str],
        ) -> Result<CubeWorkspaceLease> {
            unimplemented!()
        }
        async fn create_change(&self, _: &std::path::Path, _: &str) -> Result<CubeChangeHandle> {
            unimplemented!()
        }
        async fn goto_workspace(&self, _: &std::path::Path, _: u64) -> Result<()> {
            unimplemented!()
        }
        async fn release_workspace(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn workspace_status(&self, _: &std::path::Path) -> Result<CubeWorkspaceStatus> {
            unimplemented!()
        }
        async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> Result<()> {
            unimplemented!()
        }
        async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
            unimplemented!()
        }
        async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
            Ok(vec![])
        }
        async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
            Ok(vec![])
        }
    }

    struct NoopRunner;

    #[async_trait]
    impl ExecutionRunner for NoopRunner {
        async fn run_execution(
            &self,
            _worker_id: &str,
            _execution: &WorkExecution,
            _work_item: &crate::work::WorkItem,
            _workspace_path: &std::path::Path,
            _cube_change_id: Option<&str>,
        ) -> Result<RunOutcome> {
            unimplemented!()
        }
    }

    /// Records every `reap_worker` call and, at reap time, snapshots
    /// whether the execution's pool slot is still claimed. The production
    /// reaper (`ServerState::release_worker_pane`) kills the OS process
    /// tree and frees the slot; this stub only records, leaving the
    /// sweep's own `release_worker_and_kick` to free the slot — so the
    /// "still claimed at reap time" snapshot proves the reap ran BEFORE
    /// the slot/lease was released (defect 2's ordering requirement).
    struct RecordingReaper {
        coordinator: Arc<ExecutionCoordinator>,
        reaped: StdMutex<Vec<(String, bool)>>,
    }

    impl RecordingReaper {
        fn new(coordinator: Arc<ExecutionCoordinator>) -> Self {
            Self {
                coordinator,
                reaped: StdMutex::new(Vec::new()),
            }
        }

        /// `(execution_id, slot_still_claimed_at_reap)` for each reap.
        fn reaped(&self) -> Vec<(String, bool)> {
            self.reaped.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl StaleWorkerReaper for RecordingReaper {
        async fn reap_worker(&self, execution_id: &str) {
            let still_claimed = self
                .coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(execution_id);
            self.reaped
                .lock()
                .unwrap()
                .push((execution_id.to_owned(), still_claimed));
        }
    }

    // ─── helpers ─────────────────────────────────────────────────────────────

    fn open_db() -> (TempDir, WorkDb) {
        let dir = TempDir::new().unwrap();
        let db = WorkDb::open(dir.path().join("state.db")).unwrap();
        (dir, db)
    }

    fn create_product(db: &WorkDb) -> String {
        db.create_product(CreateProductInput {
            name: "test-product".to_owned(),
            description: None,
            repo_remote_url: Some("https://github.com/test/repo".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap()
        .id
    }

    fn create_active_chore(db: &WorkDb, product_id: &str) -> String {
        let chore = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product_id)
                    .name("test chore")
                    .build(),
            )
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        chore.id
    }

    /// Create a `ready` execution and stamp `started_at` to 5 minutes ago
    /// so the grace-period guard passes.
    fn create_old_execution(db: &WorkDb, work_item_id: &str) -> String {
        use boss_protocol::RequestExecutionInput;
        let execution = db
            .request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
            .unwrap();
        let old_started_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(300) as i64; // 5 minutes ago
        db.force_started_at_for_test(&execution.id, old_started_at).unwrap();
        execution.id
    }

    fn make_coordinator(db: Arc<WorkDb>, pool_size: usize) -> Arc<ExecutionCoordinator> {
        Arc::new(ExecutionCoordinator::new(
            db,
            WorkerPool::new(pool_size),
            Arc::new(NoopCube),
            Arc::new(NoopRunner),
        ))
    }

    fn register_slot(live_states: &LiveWorkerStateRegistry, slot_id: u8, execution_id: &str, work_item_id: &str) {
        live_states.register_spawn(
            slot_id,
            execution_id,
            "claude-opus-4-7",
            std::process::id() as i32,
            Some(WorkItemBinding {
                work_item_id: work_item_id.to_owned(),
                work_item_name: "test chore".to_owned(),
                execution_id: execution_id.to_owned(),
            }),
        );
    }

    /// Drive a slot to `Working` with NO tool in flight (a balanced
    /// PreToolUse/PostToolUse pair). `last_event_at` is stamped "now".
    fn drive_to_working_idle(live_states: &LiveWorkerStateRegistry, slot_id: u8) {
        live_states.apply_event(
            slot_id,
            &WorkerEvent::PreToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
        );
        live_states.apply_event(
            slot_id,
            &WorkerEvent::PostToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
                tool_response: serde_json::json!({}),
            },
        );
    }

    /// Drive a slot to `Working` WITH a tool in flight (PreToolUse only,
    /// no balancing PostToolUse) — models a long foreground bazel build.
    fn drive_to_working_tool_in_flight(live_states: &LiveWorkerStateRegistry, slot_id: u8) {
        live_states.apply_event(
            slot_id,
            &WorkerEvent::PreToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
        );
    }

    // ─── tests ───────────────────────────────────────────────────────────────

    /// The core invariant: a `working`, tool-idle slot whose last hook is
    /// older than the threshold has its execution orphaned, its pool slot
    /// released, and a `stale_worker_reconcile` event emitted.
    #[tokio::test]
    async fn stale_idle_worker_is_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        drive_to_working_idle(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;
        let claimed_before = coordinator.worker_pool().claimed_execution_ids().await;
        assert!(claimed_before.contains(&execution_id));

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(outcome.reaped, 1, "stale idle worker must be reaped");

        let exec = db.get_execution(&execution_id).unwrap();
        assert_eq!(exec.status, ExecutionStatus::Orphaned);

        let claimed_after = coordinator.worker_pool().claimed_execution_ids().await;
        assert!(
            !claimed_after.contains(&execution_id),
            "pool slot must be released after reap",
        );

        // Defect 2: the worker's process tree must be reaped, and the reap
        // must run BEFORE the pool slot / cube lease is released. The
        // recording reaper snapshots the slot as still-claimed at reap
        // time, which pins the reap-before-release ordering.
        assert_eq!(
            reaper.reaped(),
            vec![(execution_id.clone(), true)],
            "reconcile must reap the process tree before releasing the slot/lease",
        );

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "stale_worker_reconcile");
        assert_eq!(events[0].outcome, "ok");
        assert_eq!(events[0].work_item_id.as_deref(), Some(work_item_id.as_str()));

        let item = db.get_work_item(&work_item_id).unwrap();
        let desc = match &item {
            boss_protocol::WorkItem::Chore(t) | boss_protocol::WorkItem::Task(t) => t.description.clone(),
            _ => panic!("expected chore"),
        };
        assert!(desc.contains("[engine-reconcile]"), "got: {desc:?}");
    }

    /// A `working` slot whose last hook is *recent* (within the
    /// threshold) is left alone — the common healthy case.
    #[tokio::test]
    async fn fresh_worker_is_not_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        drive_to_working_idle(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            NEVER_STALE,
        )
        .await;

        assert_eq!(outcome.reaped, 0, "fresh worker must not be reaped");
        assert_eq!(outcome.fresh_skipped, 1);
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
    }

    /// A `working` slot WITH a tool in flight (e.g. a long foreground
    /// bazel build) is never reaped even past the threshold — this is the
    /// critical false-positive guard.
    #[tokio::test]
    async fn worker_with_tool_in_flight_is_not_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        drive_to_working_tool_in_flight(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(
            outcome.reaped, 0,
            "a tool in flight (long foreground build) must never be reaped",
        );
        assert_eq!(outcome.tool_in_flight_skipped, 1);
        assert!(sink.events().await.is_empty());
    }

    /// A slot that is still `Spawning` (no working transition yet) is not
    /// a candidate.
    #[tokio::test]
    async fn non_working_activity_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        // Left at Spawning — no events applied.

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(outcome.reaped, 0);
        assert_eq!(outcome.not_working_skipped, 1);
    }

    /// A stale-looking `working` slot whose execution started within the
    /// grace window is skipped, guarding against racing a fresh dispatch.
    #[tokio::test]
    async fn recent_started_at_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        use boss_protocol::RequestExecutionInput;
        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .build(),
            )
            .unwrap();
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        db.force_started_at_for_test(&execution.id, now_secs).unwrap();

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution.id, &work_item_id);
        drive_to_working_idle(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution.id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(outcome.reaped, 0, "grace period must prevent reaping fresh dispatches");
        assert_eq!(outcome.grace_skipped, 1);
    }

    /// Regression test (a) for the false-positive cancel: slot reuse.
    ///
    /// Slot 1 was recycled — its live-state `run_id` now points at the
    /// CURRENT execution, but its `last_event_at` still carries a PRIOR
    /// run's (much older) timestamp, the recycled-slot attribution
    /// artifact from the incident. The current execution started AFTER
    /// that stale timestamp. Even at an always-stale threshold, the
    /// reconciler must NOT reap: a hook timestamp predating the
    /// execution's own `started_at` cannot be one of its events, so
    /// staleness is un-evaluable and the (healthy) worker is left alone.
    /// Without the event-attribution guard this slot would be reaped,
    /// false-cancelling a live worker.
    #[tokio::test]
    async fn slot_reuse_stale_prior_event_is_not_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        // The CURRENT execution started 5 minutes ago (clears the grace
        // window) — `create_old_execution` stamps started_at to now-300.
        let execution_id = create_old_execution(&db, &work_item_id);
        let started_epoch = db
            .get_execution(&execution_id)
            .unwrap()
            .started_at
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap();

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        drive_to_working_idle(&live_states, 1);
        // The recycled slot carries a PRIOR run's last-event timestamp,
        // an hour before THIS execution even started — the exact
        // mis-attribution the incident hit (last_event_at "03:43:55Z"
        // predating the 06:24Z dispatch).
        live_states.set_last_event_at_for_test(1, iso8601_utc(started_epoch - 3_600));

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(
            outcome.reaped, 0,
            "a worker whose only 'staleness' is a recycled-slot prior-run timestamp must not be reaped",
        );
        assert_eq!(outcome.pre_start_event_skipped, 1);
        // Execution untouched, slot still claimed, no reap, no event.
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
        assert!(
            coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(&execution_id),
            "the healthy worker's slot must remain claimed",
        );
        assert!(
            reaper.reaped().is_empty(),
            "no process reap may fire for a healthy worker"
        );
        assert!(sink.events().await.is_empty());
    }

    /// Regression test (b): a legitimate reconcile-cancel must reap the
    /// worker's process tree BEFORE the slot/lease is freed. The
    /// recording reaper captures the pool slot as still-claimed at reap
    /// time; combined with the slot being released by the end of the
    /// pass, that pins the reap-before-release ordering the incident
    /// required (lease freed while the process lived is what produced the
    /// shared-workspace catastrophe).
    #[tokio::test]
    async fn reconcile_reaps_process_before_releasing_slot() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        drive_to_working_idle(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(outcome.reaped, 1);
        // Exactly one reap, for this execution, observed while the slot
        // was STILL claimed → reap ran before the slot/lease release.
        assert_eq!(reaper.reaped(), vec![(execution_id.clone(), true)]);
        // …and by the end of the pass the slot is released.
        assert!(
            !coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(&execution_id),
            "slot must be released after the reap",
        );
    }
}
