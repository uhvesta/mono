//! Periodic reconciler that releases worker-pool claims that have
//! outlived their execution.
//!
//! ## The leak this closes
//!
//! A [`crate::coordinator::WorkerPool`] slot is claimed at dispatch
//! (`drain_ready_queue` / `force_dispatch` → `claim_worker`) and, for a
//! pane-spawned run, its release is DEFERRED to
//! [`crate::app::ServerState::release_worker_pane`], which frees the
//! slot only when the macOS app tears the libghostty pane down. Every
//! other release path keys off a *live* worker:
//!
//! * completion (`force_release` / `force_stop_execution` /
//!   `finalize_pr_transition` / `finalize_automation_triage`) only frees
//!   the slot as a side effect of `release_worker_pane`, and only when
//!   the run→slot mapping is still registered;
//! * the dead-pid, stale-worker, and transient-recovery sweeps all
//!   iterate [`LiveWorkerStateRegistry`] and derive the slot to release
//!   from a *live-state entry*.
//!
//! Nothing iterates the pool's OWN claimed slots. So a slot claimed by
//! an execution that reached a terminal state WITHOUT a live pane —
//! a mid-spawn cancel (claim taken, no slot registered yet), a
//! `finalize_pr_transition` DB-error early-return, a teardown that
//! dropped the run→slot mapping but not the pool claim, or a
//! `bossctl agents stop` that released the cube lease but not the
//! claim — is released by NOTHING. The claim outlives its execution
//! forever. Once all [`MAX_AUTOMATION_POOL_SIZE`] automation slots leak
//! this way, `claim_worker` returns `None` for every automation
//! dispatch and the whole automation subsystem is wedged with no
//! self-healing path short of an engine restart.
//!
//! ## Algorithm
//!
//! For each pool (main and automation), snapshot the pool's own claimed
//! slots via [`WorkerPool::claims`] and, for each `(worker_id,
//! execution_id)`:
//!
//! 1. If a [`LiveWorkerStateRegistry`] entry still backs the claim (a
//!    live `run_id == execution_id`), SKIP — a live pane owns the slot
//!    and the completion / dead-pid / stale-worker paths own its
//!    teardown. Releasing it here would let a fresh dispatch hit
//!    `SpawnWorkerPane` `SlotBusy` against a pane that is still up.
//! 2. Look up the execution. On a DB error, SKIP this pass (conservative
//!    — a transient error is not proof the row is gone).
//! 3. If the execution is NOT terminal, SKIP — the slot is legitimately
//!    held (claimed at dispatch, spawn in flight, or a live run).
//! 4. If the execution terminated within the last [`LEAK_GRACE_SECS`],
//!    SKIP — a legitimate teardown (e.g. `run_execution`'s tail, which
//!    releases the pool slot *unconditionally* after a mid-spawn cancel)
//!    may still be in flight, and racing it could double-release a slot
//!    a fresh dispatch has just re-claimed. The reconciler is a backstop
//!    for claims stuck for a while, not the happy path.
//! 5. Terminal execution + no live pane + past the grace = a leaked
//!    claim. Release it via a compare-and-release
//!    ([`ExecutionCoordinator::release_pool_claim_if_execution`]) so a
//!    re-claim race can't yank a fresh, live claim, then emit a
//!    `pool_claim_reconcile` dispatch event and kick the scheduler.
//!
//! ## Why the live-state cross-check is sound
//!
//! `release_worker_pane` frees the pool slot BEFORE it drops the
//! live-state entry (`app.rs`: `release_worker_and_kick` then
//! `live_worker_states.release_slot`). So during normal teardown the
//! observable states are "claimed + live entry" → "free + live entry" →
//! "free + no entry" — never "claimed + no entry". A "claimed + no live
//! entry" slot is therefore always a genuine leak, never a teardown
//! in flight.
//!
//! ## Cadence
//!
//! Runs every [`DEFAULT_INTERVAL`] and fires once immediately on boot
//! (same pattern as [`crate::dead_pid_sweep`]) so a pool left wedged by
//! a crash self-heals at engine startup without an operator restart.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::coordinator::ExecutionCoordinator;
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::work::WorkDb;

/// How often the pool-claim reconciler runs. 60s mirrors the dead-pid
/// and stale-worker sweeps — fast enough that a leaked automation slot
/// is reclaimed within a minute, slow enough to be negligible overhead.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// Grace period after an execution's `finished_at` during which a still-
/// claimed slot is left alone. A terminal execution's pool slot is
/// normally freed within milliseconds (completion's `release_worker_pane`
/// or `run_execution`'s unconditional tail release after a mid-spawn
/// cancel). The reconciler only steps in once a claim has outlived its
/// execution by longer than this — so it never races the happy-path
/// teardown and double-releases a slot a fresh dispatch just re-claimed.
/// A terminal execution with no parseable `finished_at` (a data anomaly —
/// every terminal path stamps it) is treated as past the grace.
pub const LEAK_GRACE_SECS: i64 = 60;

/// Counts from one sweep pass; logged at `info` when any claim was
/// released.
#[derive(Debug, Default, bon::Builder)]
pub struct PoolClaimSweepOutcome {
    /// Leaked claims (terminal execution, no live pane) that were freed.
    pub released: usize,
    /// Claims left alone because a live worker pane still backs them.
    pub live_backed_skipped: usize,
    /// Claims left alone because the execution is still non-terminal.
    pub non_terminal_skipped: usize,
    /// Terminal claims left alone because the execution terminated within
    /// [`LEAK_GRACE_SECS`] — a legitimate teardown may still be in flight.
    pub grace_skipped: usize,
    /// Claims skipped this pass because the execution lookup failed
    /// (conservative — retried next pass).
    pub lookup_failed_skipped: usize,
    /// Claims that lost the compare-and-release race (freed or re-claimed
    /// by a live execution between snapshot and release). Benign.
    pub race_skipped: usize,
}

impl PoolClaimSweepOutcome {
    fn has_activity(&self) -> bool {
        self.released > 0
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn so a pool wedged before an engine restart
/// self-heals at boot without waiting for the first interval.
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
                    released = outcome.released,
                    live_backed_skipped = outcome.live_backed_skipped,
                    non_terminal_skipped = outcome.non_terminal_skipped,
                    grace_skipped = outcome.grace_skipped,
                    race_skipped = outcome.race_skipped,
                    "pool-claim sweep: released leaked worker-pool claim(s)",
                );
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// Run a single pool-claim reconciliation pass over both pools. Returns
/// a summary of what happened; callers may log it.
///
/// Takes `coordinator` as `Arc` because releasing a claim kicks the
/// scheduler, which spawns a task that holds a reference.
pub async fn run_one_pass(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
) -> PoolClaimSweepOutcome {
    let mut outcome = PoolClaimSweepOutcome::default();

    let now_epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let grace_cutoff = now_epoch_secs - LEAK_GRACE_SECS;

    // Executions that currently have a live worker pane. `run_id` on a
    // live-state entry IS the execution id (see dead_pid_sweep). A claim
    // whose execution is in this set is still owned by a live pane and
    // its teardown path — leave it alone.
    let live_run_ids: HashSet<String> = live_states
        .snapshot()
        .into_iter()
        .map(|state| state.run_id)
        .collect();

    for (pool, pool_name) in [
        (coordinator.worker_pool(), "main"),
        (coordinator.automation_worker_pool(), "automation"),
        (coordinator.review_worker_pool(), "review"),
    ] {
        for claim in pool.claims().await {
            // A live pane still backs this slot — the completion /
            // dead-pid / stale-worker paths own the release. Releasing
            // here would race a pane that may still be physically up.
            if live_run_ids.contains(&claim.execution_id) {
                outcome.live_backed_skipped += 1;
                continue;
            }

            let execution = match work_db.get_execution(&claim.execution_id) {
                Ok(execution) => execution,
                Err(err) => {
                    // A transient DB error is not proof the row is gone.
                    // Skip and retry next pass rather than free a
                    // possibly-live claim. (Mirrors dead_pid_sweep's
                    // conservative skip.)
                    tracing::warn!(
                        worker_id = %claim.worker_id,
                        execution_id = %claim.execution_id,
                        pool = pool_name,
                        ?err,
                        "pool-claim sweep: failed to look up claimed execution; skipping this pass",
                    );
                    outcome.lookup_failed_skipped += 1;
                    continue;
                }
            };

            if !execution_status_is_terminal(&execution.status) {
                // Legitimately held: claimed at dispatch with the spawn
                // still in flight, or a live run. Not our job.
                outcome.non_terminal_skipped += 1;
                continue;
            }

            // Grace guard: a freshly-terminalized claim may still be
            // mid-teardown by the path that owns it (e.g. run_execution's
            // unconditional tail release after a mid-spawn cancel).
            // Release only once it has been stuck past the grace, so we
            // never race the happy path. A missing/unparseable
            // finished_at (data anomaly — every terminal path stamps it)
            // falls through as past-grace.
            let finished_epoch = execution
                .finished_at
                .as_deref()
                .and_then(|s| s.parse::<i64>().ok());
            if matches!(finished_epoch, Some(t) if t > grace_cutoff) {
                outcome.grace_skipped += 1;
                continue;
            }

            tracing::warn!(
                worker_id = %claim.worker_id,
                execution_id = %claim.execution_id,
                pool = pool_name,
                execution_status = %execution.status,
                "pool-claim sweep: slot claimed by terminal execution with no live worker pane; \
                 releasing leaked claim",
            );

            let released = coordinator
                .release_pool_claim_if_execution(&claim.worker_id, &claim.execution_id)
                .await;

            if !released {
                // Lost the compare-and-release race: the slot was freed
                // or re-claimed by a live execution between the snapshot
                // and now. Benign — nothing to do.
                outcome.race_skipped += 1;
                continue;
            }

            outcome.released += 1;
            dispatch_events
                .emit(
                    DispatchEvent::new(
                        Stage::PoolClaimReconcile,
                        Outcome::Ok,
                        &claim.execution_id,
                    )
                    .with_work_item(&execution.work_item_id)
                    .with_worker(&claim.worker_id)
                    .with_details(serde_json::json!({
                        "pool": pool_name,
                        "worker_id": claim.worker_id,
                        "execution_status": execution.status,
                    })),
                )
                .await;
        }
    }

    outcome
}

/// The terminal execution statuses. A claim held by an execution in any
/// of these states, with no live pane, is leaked. Kept in sync with the
/// same predicate in the other sweeps and `WorkDb`.
fn execution_status_is_terminal(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "abandoned" | "cancelled" | "orphaned"
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use boss_protocol::{WorkExecution, WorkItemBinding};
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
        async fn create_change(&self, _: &std::path::PathBuf, _: &str) -> Result<CubeChangeHandle> {
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

    fn create_active_chore(db: &WorkDb, product_id: &str, name: &str) -> String {
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product_id.to_owned(),
                name: name.to_owned(),
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

    fn create_execution(db: &WorkDb, work_item_id: &str) -> String {
        use boss_protocol::RequestExecutionInput;
        db.request_execution(
            RequestExecutionInput::builder()
                .work_item_id(work_item_id)
                .build(),
        )
        .unwrap()
        .id
    }

    /// Raw UPDATE to drive an execution to `completed` — exercises the
    /// completion-path terminal status without a full running-run setup.
    fn force_completed(db: &WorkDb, execution_id: &str) {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            rusqlite::params![execution_id],
        )
        .unwrap();
    }

    /// Stamp `finished_at` to `secs_ago` seconds in the past so the
    /// leak-grace guard treats the claim as genuinely stuck (the terminal
    /// paths stamp `finished_at = now`, which is inside the grace).
    fn age_finished_at(db: &WorkDb, execution_id: &str, secs_ago: i64) {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            - secs_ago;
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET finished_at = ?2 WHERE id = ?1",
            rusqlite::params![execution_id, epoch.to_string()],
        )
        .unwrap();
    }

    /// Build an `Arc<ExecutionCoordinator>` whose main pool has
    /// `main_pool_size` slots and whose automation pool is the default
    /// full 3-slot pool.
    fn make_coordinator(db: Arc<WorkDb>, main_pool_size: usize) -> Arc<ExecutionCoordinator> {
        Arc::new(ExecutionCoordinator::new(
            db,
            WorkerPool::new(main_pool_size),
            Arc::new(NoopCube),
            Arc::new(NoopRunner),
        ))
    }

    fn register_live_pane(live_states: &LiveWorkerStateRegistry, slot_id: u8, execution_id: &str) {
        live_states.register_spawn(
            slot_id,
            execution_id,
            "claude-opus-4-8",
            std::process::id() as i32,
            Some(WorkItemBinding {
                work_item_id: "wi".to_owned(),
                work_item_name: "chore".to_owned(),
                execution_id: execution_id.to_owned(),
            }),
        );
    }

    // ─── tests ───────────────────────────────────────────────────────────────

    /// The core regression: claim all 3 automation slots, terminate each
    /// holder via a DIFFERENT terminal path (orphaned / cancelled /
    /// completed), and assert the sweep returns the pool to 0/3 (so a new
    /// triage can dispatch) and emits one `pool_claim_reconcile` event
    /// per freed slot.
    #[tokio::test]
    async fn frees_every_leaked_automation_claim_across_terminal_paths() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let db = Arc::new(db);

        let coordinator = make_coordinator(db.clone(), 0);
        let pool = coordinator.automation_worker_pool();

        // Three leaked claims, each terminated via a distinct path.
        let exec_orphaned = create_execution(&db, &create_active_chore(&db, &product_id, "a"));
        let exec_cancelled = create_execution(&db, &create_active_chore(&db, &product_id, "b"));
        let exec_completed = create_execution(&db, &create_active_chore(&db, &product_id, "c"));

        for exec in [&exec_orphaned, &exec_cancelled, &exec_completed] {
            let worker_id = pool.claim_worker(exec, None).await.unwrap();
            assert!(worker_id.starts_with("auto-worker-"));
        }
        assert_eq!(pool.idle_count().await, 0, "pool must start fully claimed");

        // Terminate the holders, one per terminal path, then age each
        // past the leak grace (the terminal paths stamp finished_at=now).
        db.mark_execution_orphaned(&exec_orphaned, "test orphan").unwrap();
        assert!(db.cancel_running_execution(&exec_cancelled).unwrap());
        force_completed(&db, &exec_completed);
        for exec in [&exec_orphaned, &exec_cancelled, &exec_completed] {
            age_finished_at(&db, exec, 300);
        }

        // No live-state entries — this is the documented "3/3 busy, zero
        // live workers" wedge.
        let live_states = LiveWorkerStateRegistry::new();
        let sink = Arc::new(RecordingDispatchEventSink::new());

        let outcome =
            run_one_pass(db.as_ref(), &live_states, coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.released, 3, "all three leaked claims must be freed");
        assert_eq!(outcome.live_backed_skipped, 0);
        assert_eq!(outcome.non_terminal_skipped, 0);

        assert_eq!(
            pool.idle_count().await,
            3,
            "automation pool must be fully idle after the sweep — dispatch unwedged",
        );
        assert!(
            pool.claimed_execution_ids().await.is_empty(),
            "no claims may remain",
        );

        // One pool_claim_reconcile event per freed slot, carrying the
        // worker_id and terminal status so the leak is diagnosable.
        let events = sink.events().await;
        assert_eq!(events.len(), 3, "expected one event per released claim");
        for event in &events {
            assert_eq!(event.stage, "pool_claim_reconcile");
            assert_eq!(event.outcome, "ok");
            assert_eq!(event.details["pool"], "automation");
            assert!(event.worker_id.as_deref().unwrap().starts_with("auto-worker-"));
        }
    }

    /// A claim whose execution is still non-terminal (a legitimately held
    /// slot — claimed at dispatch, spawn in flight) is left alone.
    #[tokio::test]
    async fn leaves_non_terminal_claims_alone() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let db = Arc::new(db);

        let coordinator = make_coordinator(db.clone(), 0);
        let pool = coordinator.automation_worker_pool();

        let exec = create_execution(&db, &create_active_chore(&db, &product_id, "a"));
        let worker_id = pool.claim_worker(&exec, None).await.unwrap();

        // Execution left in `ready` (non-terminal).
        let live_states = LiveWorkerStateRegistry::new();
        let sink = Arc::new(RecordingDispatchEventSink::new());

        let outcome =
            run_one_pass(db.as_ref(), &live_states, coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.released, 0);
        assert_eq!(outcome.non_terminal_skipped, 1);
        assert!(
            pool.claimed_execution_ids().await.contains(&exec),
            "non-terminal claim must remain held",
        );
        assert!(sink.events().await.is_empty());
        let _ = worker_id;
    }

    /// A terminal execution that STILL has a live worker pane is left to
    /// the completion / dead-pid / stale-worker paths — releasing it here
    /// would race a pane that may still be physically up (SlotBusy).
    #[tokio::test]
    async fn leaves_live_backed_claims_to_the_completion_path() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let db = Arc::new(db);

        let coordinator = make_coordinator(db.clone(), 0);
        let pool = coordinator.automation_worker_pool();

        let exec = create_execution(&db, &create_active_chore(&db, &product_id, "a"));
        let worker_id = pool.claim_worker(&exec, None).await.unwrap();
        // auto-worker-1 → slot 9.
        db.mark_execution_orphaned(&exec, "terminal but pane still up")
            .unwrap();

        let live_states = LiveWorkerStateRegistry::new();
        register_live_pane(&live_states, 9, &exec);
        let sink = Arc::new(RecordingDispatchEventSink::new());

        let outcome =
            run_one_pass(db.as_ref(), &live_states, coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.released, 0, "live-backed claim must not be released");
        assert_eq!(outcome.live_backed_skipped, 1);
        assert!(
            pool.claimed_execution_ids().await.contains(&exec),
            "live-backed claim must remain held",
        );
        assert!(sink.events().await.is_empty());
        let _ = worker_id;
    }

    /// A claim whose execution went terminal just now (within the leak
    /// grace) is left alone — a legitimate teardown may still be in
    /// flight; releasing it could race `run_execution`'s unconditional
    /// tail release and double-free a re-claimed slot.
    #[tokio::test]
    async fn leaves_freshly_terminalized_claims_within_grace() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let db = Arc::new(db);

        let coordinator = make_coordinator(db.clone(), 0);
        let pool = coordinator.automation_worker_pool();

        let exec = create_execution(&db, &create_active_chore(&db, &product_id, "a"));
        pool.claim_worker(&exec, None).await.unwrap();
        // Terminal, finished_at = now (inside the grace window).
        db.mark_execution_orphaned(&exec, "just terminated").unwrap();

        let live_states = LiveWorkerStateRegistry::new();
        let sink = Arc::new(RecordingDispatchEventSink::new());

        let outcome =
            run_one_pass(db.as_ref(), &live_states, coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.released, 0, "fresh terminal claim must wait out the grace");
        assert_eq!(outcome.grace_skipped, 1);
        assert!(
            pool.claimed_execution_ids().await.contains(&exec),
            "claim must remain held during the grace",
        );
        assert!(sink.events().await.is_empty());
    }

    /// A leaked MAIN-pool claim is also reconciled (the sweep walks both
    /// pools), and the compare-and-release is idempotent across passes.
    #[tokio::test]
    async fn frees_main_pool_claim_and_is_idempotent() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let db = Arc::new(db);

        let coordinator = make_coordinator(db.clone(), 2);
        let pool = coordinator.worker_pool();

        let exec = create_execution(&db, &create_active_chore(&db, &product_id, "a"));
        let worker_id = pool.claim_worker(&exec, None).await.unwrap();
        assert!(worker_id.starts_with("worker-"));
        db.mark_execution_orphaned(&exec, "test orphan").unwrap();
        age_finished_at(&db, &exec, 300);

        let live_states = LiveWorkerStateRegistry::new();
        let sink = Arc::new(RecordingDispatchEventSink::new());

        let first =
            run_one_pass(db.as_ref(), &live_states, coordinator.clone(), sink.as_ref()).await;
        assert_eq!(first.released, 1);
        assert_eq!(pool.idle_count().await, 2, "main pool fully idle after release");

        // Second pass: nothing left to release.
        let second =
            run_one_pass(db.as_ref(), &live_states, coordinator.clone(), sink.as_ref()).await;
        assert_eq!(second.released, 0);
        assert_eq!(sink.events().await.len(), 1, "no duplicate event on the idempotent pass");
    }
}
