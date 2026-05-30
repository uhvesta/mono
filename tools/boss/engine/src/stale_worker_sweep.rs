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
//!    a. Skip unless `activity == Working`. `Spawning` is still coming
//!       up; `Idle`/`WaitingForInput` are handled by the completion and
//!       transient-recovery paths; `Terminated`/`Errored` are done.
//!    b. Skip if a tool is in flight (`current_tool.is_some()`). A
//!       *foreground* `bazel build //...` on a cold cache legitimately
//!       runs for many minutes with no intervening hook — reaping it
//!       would be the regression we must not cause. The companion fix
//!       (the pre-push gate's `timeout` guidance) bounds that case; this
//!       sweep deliberately only targets the *idle-between-tools* wedge.
//!    c. Skip if `last_event_at` is newer than the staleness threshold,
//!       or absent (no hook has landed yet).
//!    d. Age guard against the DB `started_at` (skip fresh dispatches).
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

use crate::coordinator::{ExecutionCoordinator, WorkerPool};
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

/// Counts from one pass of the sweep; logged at `info` when a reap
/// occurs.
#[derive(Debug, Default)]
pub struct StaleWorkerSweepOutcome {
    pub reaped: usize,
    pub fresh_skipped: usize,
    pub tool_in_flight_skipped: usize,
    pub not_working_skipped: usize,
    pub grace_skipped: usize,
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
        if execution_status_is_terminal(&execution.status) {
            continue;
        }

        // Grace-period guard: skip executions whose `started_at` is
        // within STALE_GRACE_SECS or not yet recorded.
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
        if let Some(work_item_id) = &state.work_item_id {
            if let Err(err) = append_reconcile_audit(
                work_db,
                work_item_id,
                execution_id,
                now_epoch_secs,
                stale_threshold_secs,
                recovery_patch.as_deref(),
            ) {
                tracing::warn!(
                    work_item_id,
                    ?err,
                    "stale-worker sweep: failed to append audit line to description (non-fatal)",
                );
            }
        }

        // Release the worker pool slot so the orphan sweep detects the
        // chore and creates a fresh ready execution for redispatch.
        let worker_id = WorkerPool::worker_id_for_slot(state.slot_id);
        coordinator
            .worker_pool()
            .release_worker(&worker_id, None)
            .await;

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

        // Wake the scheduler so it finds the newly-freed slot and the
        // chore's orphaned execution on the next tick.
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
    stale_execution_id: &str,
    now_epoch_secs: i64,
    stale_threshold_secs: i64,
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
    use boss_protocol::{WorkItemBinding, WorkerEvent};
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

    /// Create a `ready` execution and stamp `started_at` to 5 minutes ago
    /// so the grace-period guard passes.
    fn create_old_execution(db: &WorkDb, work_item_id: &str) -> String {
        use boss_protocol::RequestExecutionInput;
        let execution = db
            .request_execution(RequestExecutionInput {
                work_item_id: work_item_id.to_owned(),
                priority: None,
                preferred_workspace_id: None,
                force: false,
            })
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

    fn register_slot(
        live_states: &LiveWorkerStateRegistry,
        slot_id: u8,
        execution_id: &str,
        work_item_id: &str,
    ) {
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
        coordinator
            .worker_pool()
            .claim_worker(&execution_id, None)
            .await;
        let claimed_before = coordinator.worker_pool().claimed_execution_ids().await;
        assert!(claimed_before.contains(&execution_id));

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(outcome.reaped, 1, "stale idle worker must be reaped");

        let exec = db.get_execution(&execution_id).unwrap();
        assert_eq!(exec.status, "orphaned");

        let claimed_after = coordinator.worker_pool().claimed_execution_ids().await;
        assert!(
            !claimed_after.contains(&execution_id),
            "pool slot must be released after reap",
        );

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "stale_worker_reconcile");
        assert_eq!(events[0].outcome, "ok");
        assert_eq!(events[0].work_item_id.as_deref(), Some(work_item_id.as_str()));

        let item = db.get_work_item(&work_item_id).unwrap();
        let desc = match &item {
            boss_protocol::WorkItem::Chore(t) | boss_protocol::WorkItem::Task(t) => {
                t.description.clone()
            }
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
        coordinator
            .worker_pool()
            .claim_worker(&execution_id, None)
            .await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            NEVER_STALE,
        )
        .await;

        assert_eq!(outcome.reaped, 0, "fresh worker must not be reaped");
        assert_eq!(outcome.fresh_skipped, 1);
        assert_eq!(db.get_execution(&execution_id).unwrap().status, "ready");
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
        coordinator
            .worker_pool()
            .claim_worker(&execution_id, None)
            .await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
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
        coordinator
            .worker_pool()
            .claim_worker(&execution_id, None)
            .await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
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
            .request_execution(RequestExecutionInput {
                work_item_id: work_item_id.clone(),
                priority: None,
                preferred_workspace_id: None,
                force: false,
            })
            .unwrap();
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        db.force_started_at_for_test(&execution.id, now_secs).unwrap();

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution.id, &work_item_id);
        drive_to_working_idle(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator
            .worker_pool()
            .claim_worker(&execution.id, None)
            .await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(outcome.reaped, 0, "grace period must prevent reaping fresh dispatches");
        assert_eq!(outcome.grace_skipped, 1);
    }
}
