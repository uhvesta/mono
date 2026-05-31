//! Periodic reconciler that detects and reaps active worker slots
//! whose underlying OS process has died.
//!
//! Complements the orphan-active sweep in [`crate::orphan_sweep`]. The
//! orphan sweep detects chores in `active` status with no live
//! execution in the worker pool. This sweep detects chores whose
//! execution IS still claimed in the pool, but the backing OS process
//! is dead (killed, OOM, crash). Without this, a kill-9'd worker
//! leaves the pool slot claimed forever and the orphan sweep skips the
//! chore ("already claimed"), leaving it stuck in Doing indefinitely.
//!
//! ## Algorithm
//!
//! 1. Snapshot [`crate::live_worker_state::LiveWorkerStateRegistry`] to
//!    get every active slot's `(slot_id, run_id, shell_pid, activity)`.
//! 2. For each slot with `shell_pid > 0` and non-terminal activity:
//!    a. Look up the execution in the DB (age guard: skip if
//!       `started_at` is within [`DEAD_PID_GRACE_SECS`] seconds or
//!       `None`, to avoid racing a fresh dispatch whose worker is still
//!       spinning up).
//!    b. Probe liveness via `kill(pid, 0)`:
//!       - `ESRCH` → process does not exist → proceed.
//!       - `0` (alive) or `EPERM` (alive, not ours) → skip.
//!       - Other errors → conservative skip with a warning.
//! 3. For dead PIDs:
//!    a. Mark the execution `orphaned` in the DB.
//!    b. Append an `[engine-reconcile]` audit line to the task description.
//!    c. Release the worker pool slot so the orphan sweep can redispatch.
//!    d. Emit a `dead_pid_reconcile` dispatch event.
//!    e. Kick the coordinator.
//!
//! ## False-positive guard
//!
//! The [`DEAD_PID_GRACE_SECS`] (30 s) guard skips executions whose
//! `started_at` is too recent. A worker with no `started_at` yet
//! (pane hasn't begun) is also skipped. Slow-but-running workers
//! (e.g., multi-minute bazel runs) keep their PID alive, so
//! `kill(pid, 0)` is robust against them — only `ESRCH` ("no such
//! process") triggers a reap.
//!
//! ## Cadence
//!
//! Runs every 60 seconds and fires once immediately on boot (same
//! pattern as [`crate::orphan_sweep`]).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use boss_protocol::{WorkItemPatch, WorkerActivity};

use crate::coordinator::{ExecutionCoordinator, WorkerPool};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::work::WorkDb;

/// Grace period after `started_at` (epoch seconds) during which we
/// skip PID probing. Guards against racing a fresh dispatch whose pane
/// is still spinning up and may not have fully exec'd its shell yet.
pub const DEAD_PID_GRACE_SECS: i64 = 30;

/// Counts from one pass of the sweep; logged at `info` when activity
/// occurs.
#[derive(Debug, Default)]
pub struct DeadPidSweepOutcome {
    pub reaped: usize,
    pub alive_skipped: usize,
    pub unknown_pid_skipped: usize,
    pub grace_skipped: usize,
}

impl DeadPidSweepOutcome {
    fn has_activity(&self) -> bool {
        self.reaped > 0
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn so post-crash orphans are resolved on
/// engine boot without waiting for the first interval.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    live_states: Arc<LiveWorkerStateRegistry>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let outcome = run_one_pass(
                work_db.as_ref(),
                live_states.as_ref(),
                coordinator.clone(),
                dispatch_events.as_ref(),
            )
            .await;
            if outcome.has_activity() {
                tracing::info!(
                    reaped = outcome.reaped,
                    alive_skipped = outcome.alive_skipped,
                    grace_skipped = outcome.grace_skipped,
                    "dead-pid sweep: pass complete",
                );
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// Run a single dead-PID sweep pass. Returns a summary of what
/// happened; callers may log it.
///
/// Takes `coordinator` as `Arc` because kicking the scheduler
/// requires `Arc<ExecutionCoordinator>` — the kick path spawns a
/// tokio task that holds a reference.
pub async fn run_one_pass(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
) -> DeadPidSweepOutcome {
    let mut outcome = DeadPidSweepOutcome::default();
    let snapshot = live_states.snapshot();

    let now_epoch_secs: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let grace_cutoff = now_epoch_secs - DEAD_PID_GRACE_SECS;

    for state in snapshot {
        // Skip slots with unknown PID (pane not yet reported a pid back).
        if state.shell_pid <= 0 {
            outcome.unknown_pid_skipped += 1;
            continue;
        }

        // Skip terminal slots — the completion path handles these.
        if is_terminal_activity(state.activity) {
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
                    "dead-pid sweep: failed to look up execution; skipping slot",
                );
                continue;
            }
        };

        // Skip executions already in a terminal DB state (completion
        // path may have raced the sweep).
        if execution_status_is_terminal(&execution.status) {
            continue;
        }

        // Grace-period guard: skip executions whose `started_at` is
        // within DEAD_PID_GRACE_SECS or not yet recorded. A missing
        // `started_at` means the pane hasn't fully exec'd yet.
        let started_epoch = execution
            .started_at
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok());
        match started_epoch {
            None => {
                outcome.grace_skipped += 1;
                continue;
            }
            Some(t) if t >= grace_cutoff => {
                outcome.grace_skipped += 1;
                continue;
            }
            _ => {}
        }

        // Probe PID liveness via kill(pid, 0).
        match probe_pid(state.shell_pid) {
            PidStatus::Alive | PidStatus::PermissionDenied => {
                outcome.alive_skipped += 1;
                continue;
            }
            PidStatus::Unknown(err) => {
                tracing::warn!(
                    execution_id,
                    pid = state.shell_pid,
                    error = %err,
                    "dead-pid sweep: unexpected kill(0) error; skipping conservatively",
                );
                outcome.alive_skipped += 1;
                continue;
            }
            PidStatus::Dead => {
                // Fall through to reap.
            }
        }

        tracing::info!(
            execution_id,
            work_item_id = %execution.work_item_id,
            pid = state.shell_pid,
            slot_id = state.slot_id,
            "dead-pid sweep: worker PID not found; reaping execution and releasing slot",
        );

        // Mark the execution orphaned so the DB reflects the crash and
        // bossctl agents transcript <exec-id> still works.
        let reason = format!(
            "dead-pid-reconcile: shell PID {} not found; process presumed dead",
            state.shell_pid
        );
        if let Err(err) = work_db.mark_execution_orphaned(execution_id, &reason) {
            tracing::warn!(
                execution_id,
                ?err,
                "dead-pid sweep: failed to mark execution orphaned; skipping reap",
            );
            continue;
        }

        // Snapshot the dead worker's uncommitted workspace work to a
        // durable patch before the slot is released and the workspace
        // becomes eligible for re-lease/reset. Best-effort: a failed or
        // empty capture returns None and never blocks the reap.
        let recovery_patch = crate::recovery_backup::backup_dead_execution(&execution);

        // Append [engine-reconcile] audit line to the task description
        // so a human inspecting the chore can see why it was reset (and
        // where to find the recovery patch, if one was captured).
        if let Some(work_item_id) = &state.work_item_id {
            if let Err(err) = append_reconcile_audit(
                work_db,
                work_item_id,
                execution_id,
                now_epoch_secs,
                recovery_patch.as_deref(),
            ) {
                tracing::warn!(
                    work_item_id,
                    ?err,
                    "dead-pid sweep: failed to append audit line to description (non-fatal)",
                );
            }
        }

        // Release the worker pool slot so the orphan sweep detects
        // the chore and creates a fresh ready execution for redispatch.
        let worker_id = WorkerPool::worker_id_for_slot(state.slot_id);
        coordinator
            .worker_pool()
            .release_worker(&worker_id, None)
            .await;

        // Structured event for bossctl dispatch tail.
        dispatch_events
            .emit(
                DispatchEvent::new(Stage::DeadPidReconcile, Outcome::Ok, execution_id)
                    .with_work_item(&execution.work_item_id)
                    .with_details(serde_json::json!({
                        "dead_pid": state.shell_pid,
                        "slot_id": state.slot_id,
                        "recovery_patch": recovery_patch
                            .as_deref()
                            .map(|p| p.display().to_string()),
                    })),
            )
            .await;

        // Wake the scheduler so it finds the newly-freed slot and
        // the chore's orphaned execution on the next tick.
        coordinator.kick();

        outcome.reaped += 1;
    }

    outcome
}

/// Append an `[engine-reconcile]` audit line to the work item's
/// description so an operator can see why the chore was reset.
fn append_reconcile_audit(
    work_db: &WorkDb,
    work_item_id: &str,
    dead_execution_id: &str,
    now_epoch_secs: i64,
    recovery_patch: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let item = work_db.get_work_item(work_item_id)?;
    let current_desc = match &item {
        boss_protocol::WorkItem::Product(p) => p.description.as_str(),
        boss_protocol::WorkItem::Project(p) => p.description.as_str(),
        boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => {
            t.description.as_str()
        }
    };
    let recovery_note = match recovery_patch {
        Some(path) => format!(" Uncommitted work backed up to {}.", path.display()),
        None => String::new(),
    };
    let audit_line = format!(
        "\n[engine-reconcile] epoch {now_epoch_secs}: dead worker (exec {dead_execution_id}) detected via PID probe; chore reset to todo for redispatch.{recovery_note}"
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

pub(crate) enum PidStatus {
    Alive,
    Dead,
    PermissionDenied,
    Unknown(std::io::Error),
}

/// Probe whether `pid` is alive via `kill(pid, 0)`:
/// - Returns `Alive` when the process exists and we can signal it.
/// - Returns `Dead` when `ESRCH` (no such process).
/// - Returns `PermissionDenied` when `EPERM` (process exists, not ours).
/// - Returns `Unknown` on any other error; caller skips conservatively.
pub(crate) fn probe_pid(pid: i32) -> PidStatus {
    // SAFETY: kill(pid, 0) sends no signal; it only checks whether
    // the process exists and we have permission to signal it. The
    // `pid` value comes from the OS-reported shell_pid at spawn time.
    let ret = unsafe { libc::kill(pid, 0) };
    if ret == 0 {
        return PidStatus::Alive;
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ESRCH) => PidStatus::Dead,
        Some(libc::EPERM) => PidStatus::PermissionDenied,
        _ => PidStatus::Unknown(err),
    }
}

fn is_terminal_activity(activity: WorkerActivity) -> bool {
    matches!(
        activity,
        WorkerActivity::Terminated | WorkerActivity::Errored
    )
}

fn execution_status_is_terminal(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "abandoned" | "cancelled" | "orphaned"
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;
    use async_trait::async_trait;
    use boss_protocol::WorkItemBinding;
    use tempfile::TempDir;

    use super::*;
    use crate::coordinator::{
        CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease,
        CubeWorkspaceStatus, ExecutionCoordinator, WorkerPool,
    };
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::live_worker_state::LiveWorkerStateRegistry;
    use crate::runner::{ExecutionRunner, RunOutcome};
    use crate::work::{CreateChoreInput, CreateProductInput, WorkDb, WorkItemPatch};
    use boss_protocol::WorkExecution;

    // ─── stubs ───────────────────────────────────────────────────────────────

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
        ) -> Result<CubeWorkspaceLease> {
            unimplemented!()
        }
        async fn create_change(
            &self,
            _: &std::path::PathBuf,
            _: &str,
        ) -> Result<CubeChangeHandle> {
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
            .create_chore(CreateChoreInput {
                product_id: product_id.to_owned(),
                name: "test chore".to_owned(),
                description: None,
                repo_remote_url: None,
                priority: None,
                effort_level: None,
                model_override: None,
                created_via: None,
                autostart: true,
                force_duplicate: false,
            })
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

    /// Create a `ready` execution for `work_item_id` and stamp its
    /// `started_at` to 5 minutes ago so the grace-period guard passes.
    fn create_old_execution(db: &WorkDb, work_item_id: &str) -> String {
        use boss_protocol::RequestExecutionInput;
        let execution = db
            .request_execution(RequestExecutionInput::builder()
                .work_item_id(work_item_id)
                .build())
            .unwrap();
        let old_started_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(300) as i64; // 5 minutes ago
        db.force_started_at_for_test(&execution.id, old_started_at)
            .unwrap();
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

    /// Register a slot in the live-state registry with the given PID and
    /// an optional work-item binding. Activity is left as `Spawning`
    /// (non-terminal, so the sweep considers it).
    fn register_slot_with_binding(
        live_states: &LiveWorkerStateRegistry,
        slot_id: u8,
        execution_id: &str,
        shell_pid: i32,
        work_item_id: &str,
    ) {
        live_states.register_spawn(
            slot_id,
            execution_id,
            "claude-opus-4-7",
            shell_pid,
            Some(WorkItemBinding {
                work_item_id: work_item_id.to_owned(),
                work_item_name: "test chore".to_owned(),
                execution_id: execution_id.to_owned(),
            }),
        );
    }

    /// Returns a PID that is guaranteed to not exist. Spawns the trivially
    /// short-lived `true` command, waits for it to exit, and returns its
    /// released PID. There is a narrow race where the OS could recycle the
    /// PID between `wait()` and `kill(0)`, but in practice this does not
    /// occur in test environments.
    fn dead_pid() -> i32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        let _ = child.wait(); // blocks until the process exits
        pid
    }

    // ─── tests ───────────────────────────────────────────────────────────────

    /// A slot backed by the live test process PID is never reaped, even
    /// when the grace period has passed.
    #[tokio::test]
    async fn live_pid_is_not_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_binding(
            &live_states,
            1,
            &execution_id,
            std::process::id() as i32, // self is always alive
            &work_item_id,
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator
            .worker_pool()
            .claim_worker(&execution_id, None)
            .await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome =
            run_one_pass(db.as_ref(), &live_states, coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.reaped, 0, "live PID must not be reaped");
        assert_eq!(outcome.alive_skipped, 1);
        assert!(sink.events().await.is_empty());

        let exec = db.get_execution(&execution_id).unwrap();
        assert_eq!(exec.status, "ready", "execution must be untouched when PID alive");
    }

    /// A slot with shell_pid == 0 (PID not yet reported by the app) is
    /// skipped — the pane may still be spinning up.
    #[tokio::test]
    async fn zero_pid_slot_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_binding(
            &live_states,
            1,
            &execution_id,
            0, // PID unknown
            &work_item_id,
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator
            .worker_pool()
            .claim_worker(&execution_id, None)
            .await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome =
            run_one_pass(db.as_ref(), &live_states, coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.reaped, 0, "zero PID must be skipped");
        assert_eq!(outcome.unknown_pid_skipped, 1);
    }

    /// A slot with a very recent `started_at` is skipped by the grace
    /// guard even if the PID is dead — guards against racing a fresh
    /// dispatch whose worker process hasn't fully started yet.
    #[tokio::test]
    async fn recent_started_at_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        use boss_protocol::RequestExecutionInput;
        let execution = db
            .request_execution(RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .build())
            .unwrap();
        // Stamp started_at = NOW so the grace guard fires.
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        db.force_started_at_for_test(&execution.id, now_secs)
            .unwrap();

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        // Use a definitely-dead PID; the grace guard should fire before
        // we even get to the kill(0) probe.
        let the_dead_pid = dead_pid();
        register_slot_with_binding(
            &live_states,
            1,
            &execution.id,
            the_dead_pid,
            &work_item_id,
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator
            .worker_pool()
            .claim_worker(&execution.id, None)
            .await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome =
            run_one_pass(db.as_ref(), &live_states, coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.reaped, 0, "grace period must prevent reaping fresh executions");
        assert_eq!(outcome.grace_skipped, 1);
    }

    /// A slot with no `started_at` set (pane not yet exec'd) is skipped.
    #[tokio::test]
    async fn missing_started_at_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        use boss_protocol::RequestExecutionInput;
        let execution = db
            .request_execution(RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .build())
            .unwrap();
        // Do NOT force started_at — leave it NULL.

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_binding(
            &live_states,
            1,
            &execution.id,
            dead_pid(),
            &work_item_id,
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator
            .worker_pool()
            .claim_worker(&execution.id, None)
            .await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome =
            run_one_pass(db.as_ref(), &live_states, coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.reaped, 0, "missing started_at must be treated as too fresh");
        assert_eq!(outcome.grace_skipped, 1);
    }

    /// A slot backed by a Terminated-activity live state is not touched
    /// by the sweep — the completion path handles those.
    #[tokio::test]
    async fn terminal_activity_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        live_states.register_spawn(
            1,
            &execution_id,
            "claude-opus-4-7",
            std::process::id() as i32,
            None,
        );
        // Advance to Terminated via a SessionEnd event.
        live_states.apply_event(
            1,
            &boss_protocol::WorkerEvent::SessionEnd {
                session_id: "test-session".to_owned(),
                reason: "end_turn".to_owned(),
            },
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator
            .worker_pool()
            .claim_worker(&execution_id, None)
            .await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome =
            run_one_pass(db.as_ref(), &live_states, coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.reaped, 0, "Terminated activity must not be reaped by this sweep");
    }

    /// The core invariant: a slot with a dead PID and an old enough
    /// execution has its execution marked `orphaned`, its pool slot
    /// released, and a `dead_pid_reconcile` dispatch event emitted.
    /// After the sweep, the orphan-active sweep can redispatch.
    #[tokio::test]
    async fn dead_pid_causes_orphan_and_slot_release() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let the_dead_pid = dead_pid();

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_binding(
            &live_states,
            1,
            &execution_id,
            the_dead_pid,
            &work_item_id,
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator
            .worker_pool()
            .claim_worker(&execution_id, None)
            .await;

        // Verify the slot starts claimed.
        let claimed_before = coordinator.worker_pool().claimed_execution_ids().await;
        assert!(
            claimed_before.contains(&execution_id),
            "slot must be claimed before the sweep",
        );

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome =
            run_one_pass(db.as_ref(), &live_states, coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.reaped, 1, "dead-PID execution must be reaped");
        assert_eq!(outcome.alive_skipped, 0);

        // Execution must be terminal (`orphaned`) in the DB.
        let exec = db.get_execution(&execution_id).unwrap();
        assert_eq!(
            exec.status, "orphaned",
            "execution must be marked orphaned after dead-PID reap",
        );

        // Pool slot must be free so the orphan sweep can redispatch.
        let claimed_after = coordinator.worker_pool().claimed_execution_ids().await;
        assert!(
            !claimed_after.contains(&execution_id),
            "pool slot must be released after dead-PID reap",
        );

        // A dead_pid_reconcile dispatch event must have been emitted.
        let events = sink.events().await;
        assert_eq!(events.len(), 1, "expected exactly one dispatch event");
        assert_eq!(events[0].stage, "dead_pid_reconcile");
        assert_eq!(events[0].outcome, "ok");
        assert_eq!(
            events[0].work_item_id.as_deref(),
            Some(work_item_id.as_str()),
        );

        // The task description must contain the [engine-reconcile] audit line.
        let item = db.get_work_item(&work_item_id).unwrap();
        let desc = match &item {
            boss_protocol::WorkItem::Chore(t) | boss_protocol::WorkItem::Task(t) => {
                t.description.clone()
            }
            _ => panic!("expected chore"),
        };
        assert!(
            desc.contains("[engine-reconcile]"),
            "task description must contain the engine-reconcile audit line; got: {desc:?}",
        );
    }
}
