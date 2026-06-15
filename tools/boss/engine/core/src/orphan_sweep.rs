//! Periodic reconciler that re-dispatches `active` work items with no
//! live execution — the post-crash "orphaned-in-Doing" fix.
//!
//! After an engine crash, work items that were `active` at the moment
//! of the crash stay `active` indefinitely if their executions were
//! classified as `Unknown` by the startup reconciler (no cube probe
//! signal either way). Without this module those items sit in the
//! kanban Doing column forever until a human manually runs
//! `bossctl work start <id>`.
//!
//! The sweep runs every 60 seconds and fires once immediately on
//! engine boot (same startup-sweep pattern as the merge poller). Each
//! pass:
//!
//! 1. Checks whether the worker pool has at least one idle slot; if
//!    not, returns early — a `ready` execution created now would just
//!    queue behind the full pool and can wait for the next sweep.
//! 2. Queries `active` work items whose `updated_at` is older than
//!    [`ORPHAN_MIN_AGE_SECS`] and that have no `ready` or `waiting_human`
//!    execution. `waiting_human` is a live state where the worker has parked
//!    for human input and may have released its pool slot — it must never
//!    be treated as orphaned.
//! 3. For each candidate, checks whether its latest non-terminal
//!    execution (if any) is claimed by a live worker slot. If it is,
//!    the execution is genuinely live and the candidate is skipped.
//!    As a defense-in-depth guard, any candidate whose live execution is
//!    still `waiting_human` at this point is also skipped unconditionally.
//! 4. Applies the churn guard: if the work item has already had
//!    [`ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD`] terminal executions
//!    in the last [`ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS`], it
//!    is skipped and a warning is logged.
//! 5. Calls [`WorkDb::request_execution_with_live_check`] (the same
//!    path `bossctl work start` uses) to mark the stale execution
//!    `abandoned` and insert a fresh `ready` execution, then kicks
//!    the coordinator's scheduler.
//! 6. Emits an [`Stage::OrphanActiveRedispatch`] dispatch event so
//!    the redispatch is visible in `bossctl dispatch tail`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use boss_protocol::{ExecutionKind, ExecutionStatus, RequestExecutionInput};

use crate::coordinator::ExecutionCoordinator;
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::work::{ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD, ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS, WorkDb};

/// Minimum age of `tasks.updated_at` before an active work item with
/// no live execution is treated as an orphan. Guards against racing a
/// fresh `todo → active` transition whose worker is still spinning up
/// but hasn't committed `run_started` yet.
pub const ORPHAN_MIN_AGE_SECS: i64 = 90;

/// Counts from one pass of the sweep; logged at `info` when non-zero.
#[derive(Debug, Default)]
pub struct OrphanSweepOutcome {
    pub redispatched: usize,
    pub churn_skipped: usize,
    pub no_worker_skipped: usize,
    /// Items skipped because their live execution is in `waiting_human`
    /// state. These should already be filtered by the DB query; a non-zero
    /// count here indicates a data-consistency gap worth investigating.
    pub waiting_human_skipped: usize,
    /// Items skipped because their live execution is a `running` `pr_review`
    /// (an active reviewer pane). With the union-of-pools liveness fix this
    /// should never fire; a non-zero count here means the pool snapshot did
    /// not include the review pool — worth investigating.
    pub running_reviewer_skipped: usize,
}

impl OrphanSweepOutcome {
    fn has_activity(&self) -> bool {
        self.redispatched > 0
            || self.churn_skipped > 0
            || self.waiting_human_skipped > 0
            || self.running_reviewer_skipped > 0
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn so post-crash orphans are resolved on
/// engine boot without waiting for the first interval.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let outcome = run_one_pass(work_db.as_ref(), coordinator.clone(), dispatch_events.as_ref()).await;
            if outcome.has_activity() {
                tracing::info!(
                    redispatched = outcome.redispatched,
                    churn_skipped = outcome.churn_skipped,
                    no_worker_skipped = outcome.no_worker_skipped,
                    waiting_human_skipped = outcome.waiting_human_skipped,
                    running_reviewer_skipped = outcome.running_reviewer_skipped,
                    "orphan sweep: pass complete",
                );
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// Run a single orphan-active sweep pass. Returns a summary of what
/// happened; callers may log it.
///
/// Takes `coordinator` as `Arc` because kicking the scheduler
/// requires `Arc<ExecutionCoordinator>` — the kick path spawns a
/// tokio task that holds a reference.
pub async fn run_one_pass(
    work_db: &WorkDb,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
) -> OrphanSweepOutcome {
    let mut outcome = OrphanSweepOutcome::default();

    // Fast-path: if no worker slot is free, newly-queued executions
    // would just pile up in `ready`. Skip the DB scan entirely.
    if !coordinator.worker_pool().has_idle_worker().await {
        outcome.no_worker_skipped = 1; // sentinel so callers know why we bailed
        return outcome;
    }

    // Snapshot of which execution ids are currently claimed by a live
    // worker slot across ALL pools (main, automation, review).  Built
    // once outside the per-item loop so all items in this pass see a
    // consistent view.
    //
    // Using only `worker_pool()` (the main pool) would miss executions
    // claimed in the review or automation pools — a `pr_review` reviewer
    // is claimed in `review_pool`, so a main-pool-only snapshot would
    // incorrectly treat it as dead and abandon it.
    let claimed: HashSet<String> = coordinator.all_claimed_execution_ids().await;

    let candidates = match work_db.list_orphan_active_candidates(ORPHAN_MIN_AGE_SECS) {
        Ok(ids) => ids,
        Err(err) => {
            tracing::warn!(?err, "orphan sweep: failed to list candidates; skipping pass");
            return outcome;
        }
    };

    let now_epoch_secs: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let churn_cutoff = now_epoch_secs - ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS;

    for work_item_id in candidates {
        // Churn guard: count terminal executions in the trailing window.
        let recent_terminal = match work_db.count_recent_terminal_executions(&work_item_id, churn_cutoff) {
            Ok(n) => n,
            Err(err) => {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    ?err,
                    "orphan sweep: failed to count recent terminal executions; skipping item",
                );
                continue;
            }
        };
        if recent_terminal >= ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD {
            tracing::warn!(
                work_item_id = %work_item_id,
                recent_terminal,
                threshold = ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD,
                window_secs = ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS,
                "orphan sweep: churn guard tripped; skipping redispatch — human attention required",
            );
            outcome.churn_skipped += 1;
            continue;
        }

        // Decision-point instrumentation (re-dispatch storm visibility).
        //
        // This sweep is the prime recurring re-dispatcher, so when a
        // candidate already has a *live* execution (running /
        // waiting_human) we record exactly what the sweep keyed off
        // BEFORE acting: the live execution it found and whether the
        // worker pool still claims it. The two outcomes are the whole
        // diagnosis:
        //   - live_execution_claimed = true  → the guard in
        //     `request_execution_with_live_check` returns the live row
        //     and we skip (no redispatch). The event is the proof the
        //     storm was suppressed.
        //   - live_execution_claimed = false → the pool no longer claims
        //     the live run even though its DB status is non-terminal.
        //     THIS is the smoking gun for "scheduler re-fired despite a
        //     healthy live run" — previously invisible because the
        //     dispatch pipeline only records from `request_recorded` on.
        // Only emitted when a live execution exists; a candidate with no
        // live execution is a legitimate orphan whose redispatch is
        // already covered by `orphan_active_redispatch`.
        let live_execution = work_db
            .get_live_execution_for_work_item(&work_item_id, "")
            .ok()
            .flatten();
        if let Some(live) = &live_execution {
            let live_claimed = claimed.contains(&live.id);
            dispatch_events
                .emit(
                    DispatchEvent::new(Stage::DispatchDecision, Outcome::Ok, &live.id)
                        .with_work_item(&work_item_id)
                        .with_details(serde_json::json!({
                            "loop": "orphan_active_sweep",
                            "predicate": "tasks.status='active' AND no ready execution AND \
                                          updated_at age >= ORPHAN_MIN_AGE_SECS",
                            "live_execution_id": live.id,
                            "live_execution_status": live.status,
                            "live_execution_claimed": live_claimed,
                            "recent_terminal_executions": recent_terminal,
                        })),
                )
                .await;
        }

        // Defense-in-depth: never re-dispatch a waiting_human execution even
        // if the DB exclusion above somehow let it through. waiting_human is a
        // legitimate live state — the worker parked for human input and may have
        // released its pool slot, but the execution is alive. Abandoning it would
        // clobber a live in-flight workspace and create a duplicate worker on the
        // same row.
        if let Some(live) = &live_execution
            && live.status == ExecutionStatus::WaitingHuman
        {
            tracing::warn!(
                work_item_id = %work_item_id,
                execution_id = %live.id,
                "orphan sweep: candidate has a waiting_human execution; skipping \
                 (should have been excluded by DB query — investigate)",
            );
            outcome.waiting_human_skipped += 1;
            continue;
        }

        // Defense-in-depth: a `running` pr_review execution is a live reviewer
        // pane actively working (RunWaitState::ReviewerPaneAlive). With the
        // union-of-pools fix the reviewer's review-pool claim is already in
        // `claimed`, so `request_execution_with_live_check` sees it as live
        // and returns the existing execution (non-ready → we skip below).
        // This guard fires ONLY when the reviewer is NOT in `claimed` — i.e.
        // a future pool-split scenario where the pool union missed the reviewer.
        // A non-zero `running_reviewer_skipped` count means the union failed;
        // investigate.
        if let Some(live) = &live_execution
            && live.status == ExecutionStatus::Running
            && live.kind == ExecutionKind::PrReview
            && !claimed.contains(&live.id)
        {
            tracing::warn!(
                work_item_id = %work_item_id,
                execution_id = %live.id,
                "orphan sweep: candidate has a running pr_review execution not in any pool claim \
                 (pool union failed?); skipping to protect live reviewer — investigate",
            );
            outcome.running_reviewer_skipped += 1;
            continue;
        }

        // Request a fresh execution. The `is_live` closure treats an
        // execution as live only if a worker slot currently claims it.
        // A non-terminal execution that is NOT claimed means the worker
        // died without updating the DB — `request_execution_with_live_check`
        // will mark it `abandoned` and create a new `ready` row.
        let is_live = |exec_id: &str| claimed.contains(exec_id);
        let new_execution = match work_db.request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .build(),
            is_live,
        ) {
            Ok(exec) => exec,
            Err(err) => {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    ?err,
                    "orphan sweep: failed to request execution; skipping item",
                );
                continue;
            }
        };

        // Only redispatch if we got a fresh ready execution. If the
        // existing non-terminal execution was live (claimed), the call
        // returns the existing execution with status != 'ready'.
        if new_execution.status != ExecutionStatus::Ready {
            continue;
        }

        tracing::info!(
            work_item_id = %work_item_id,
            execution_id = %new_execution.id,
            "orphan sweep: redispatching orphaned active work item",
        );

        dispatch_events
            .emit(
                DispatchEvent::new(Stage::OrphanActiveRedispatch, Outcome::Ok, &new_execution.id)
                    .with_work_item(&work_item_id)
                    .with_details(serde_json::json!({
                        "recent_terminal_executions": recent_terminal,
                    })),
            )
            .await;

        coordinator.kick();
        outcome.redispatched += 1;
    }

    outcome
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;
    use async_trait::async_trait;
    use tempfile::TempDir;

    use super::*;
    use crate::coordinator::{
        CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
        ExecutionCoordinator, WorkerPool,
    };
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::runner::{ExecutionRunner, RunOutcome};
    use crate::work::{CreateChoreInput, CreateProductInput, ExecutionStatus, WorkDb, WorkItemPatch};
    use boss_protocol::WorkExecution;

    // ─── stubs ───────────────────────────────────────────────────────────────

    struct NoopCube;

    #[async_trait]
    impl CubeClient for NoopCube {
        async fn ensure_repo(&self, _: &str) -> Result<CubeRepoHandle> {
            unimplemented!("orphan sweep tests don't invoke cube")
        }
        async fn lease_workspace(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: bool,
            _: &[&str],
        ) -> Result<CubeWorkspaceLease> {
            unimplemented!("orphan sweep tests don't invoke cube")
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
            unimplemented!("orphan sweep tests don't run executions")
        }
    }

    // ─── helpers ────────────────────────────────────────────────────────────

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
                    // repo_remote_url omitted: product already has one; the invariant
                    // disallows setting both.
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

    /// Stamp tasks.updated_at to 10 minutes ago so the age guard passes.
    fn make_old(db: &WorkDb, work_item_id: &str) {
        let old_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(600) as i64;
        db.force_updated_at_for_test(work_item_id, old_epoch).unwrap();
    }

    fn make_coordinator(db: Arc<WorkDb>, pool_size: usize) -> Arc<ExecutionCoordinator> {
        Arc::new(ExecutionCoordinator::new(
            db,
            WorkerPool::new(pool_size),
            Arc::new(NoopCube),
            Arc::new(NoopRunner),
        ))
    }

    /// Like `make_coordinator` but also installs a review pool of `review_pool_size`.
    /// Returns both the coordinator and the review pool so the caller can claim slots.
    fn make_coordinator_with_review_pool(
        db: Arc<WorkDb>,
        pool_size: usize,
        review_pool_size: usize,
    ) -> (Arc<ExecutionCoordinator>, WorkerPool) {
        let review_pool = WorkerPool::new_review(review_pool_size);
        let mut coordinator =
            ExecutionCoordinator::new(db, WorkerPool::new(pool_size), Arc::new(NoopCube), Arc::new(NoopRunner));
        coordinator.set_review_pool(review_pool.clone());
        (Arc::new(coordinator), review_pool)
    }

    // ─── tests ──────────────────────────────────────────────────────────────

    /// Orphan with NO execution → gets redispatched; dispatch event emitted.
    #[tokio::test]
    async fn redispatches_active_item_with_no_execution() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());

        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.redispatched, 1, "should have redispatched one item");

        let events = sink.events().await;
        assert_eq!(events.len(), 1, "expected exactly one dispatch event");
        assert_eq!(events[0].stage, "orphan_active_redispatch");
        assert_eq!(events[0].outcome, "ok");
        assert_eq!(events[0].work_item_id.as_deref(), Some(work_item_id.as_str()));

        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            executions.iter().any(|e| e.status == ExecutionStatus::Ready),
            "expected a ready execution after redispatch"
        );
    }

    /// Active item with a live execution claimed by a worker slot → no-op.
    #[tokio::test]
    async fn skips_item_with_live_execution() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        // Insert a ready execution and claim it in the pool — this makes
        // the item appear "already queued" (no-candidate via DB query).
        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .build(),
            )
            .unwrap();
        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution.id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        // With a `ready` execution the DB query filters the item out.
        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.redispatched, 0);
        assert!(sink.events().await.is_empty());
    }

    /// All worker slots busy → sweep returns early without touching the DB.
    #[tokio::test]
    async fn no_redispatch_when_all_workers_busy() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        make_old(&db, &work_item_id);

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker("dummy-exec-id", None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.redispatched, 0);
        assert_eq!(outcome.no_worker_skipped, 1);
        assert!(sink.events().await.is_empty());
    }

    /// Churn guard: item with ≥ threshold recent terminal executions is skipped.
    #[tokio::test]
    async fn churn_guard_skips_repeatedly_failing_item() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        make_old(&db, &work_item_id);

        let now_epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        for i in 0..ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD {
            db.insert_terminal_execution_for_test(&work_item_id, "orphaned", now_epoch - i)
                .unwrap();
        }

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.churn_skipped, 1, "churn guard should have fired");
        assert_eq!(outcome.redispatched, 0);
        assert!(sink.events().await.is_empty(), "no event on churn skip");
    }

    /// Recent-transition guard: freshly-activated item is skipped even with
    /// no execution, because its updated_at is within ORPHAN_MIN_AGE_SECS.
    #[tokio::test]
    async fn no_redispatch_for_recently_activated_item() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let _work_item_id = create_active_chore(&db, &product_id);
        // Deliberately do NOT call make_old — item's updated_at is NOW.

        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref()).await;

        assert_eq!(outcome.redispatched, 0, "should skip recently activated item");
        assert!(sink.events().await.is_empty());
    }

    /// Regression: a waiting_human execution must never be abandoned and
    /// re-dispatched by the orphan sweep. The worker parks for human input
    /// and then exits (releasing its pool slot), so the execution is not
    /// claimed — but it is still alive and waiting for a response.
    ///
    /// Previously the sweep treated unclaimed + non-terminal as "dead worker"
    /// and double-dispatched a second worker onto the same row (T1104 /
    /// exec_18b508391244f798_34 → exec_18b508565e3b6e30_39).
    #[tokio::test]
    async fn skips_item_with_waiting_human_execution() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        make_old(&db, &work_item_id);

        // Create a ready execution then force it to waiting_human to simulate
        // a worker that parked for human input and then released its slot.
        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .build(),
            )
            .unwrap();
        db.force_execution_status_for_test(&work_item_id, ExecutionStatus::WaitingHuman)
            .unwrap();

        let db = Arc::new(db);
        // Deliberately do NOT claim the execution — simulates the worker
        // process having exited after entering waiting_human.
        let coordinator = make_coordinator(db.clone(), 1);

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref()).await;

        assert_eq!(
            outcome.redispatched, 0,
            "sweep must not re-dispatch a waiting_human execution"
        );
        let events = sink.events().await;
        assert!(
            events.iter().all(|e| e.stage != "orphan_active_redispatch"),
            "no orphan_active_redispatch event should fire for waiting_human"
        );

        // The waiting_human execution must remain intact — not abandoned.
        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            executions
                .iter()
                .any(|e| e.id == execution.id && e.status == ExecutionStatus::WaitingHuman),
            "waiting_human execution must not be abandoned by the sweep"
        );
    }

    /// Regression for the critical finding in T1647's automated review:
    ///
    /// A `running` `pr_review` execution is a live reviewer pane actively
    /// working (`RunWaitState::ReviewerPaneAlive`). The reviewer is claimed
    /// in the REVIEW pool — not the MAIN pool. The old sweep only consulted
    /// `coordinator.worker_pool().claimed_execution_ids()` (the main pool),
    /// so a review-pool-claimed reviewer read as dead. The sweep would then
    /// abandon the live pr_review execution and re-dispatch a fresh
    /// chore_implementation on top of the already-pushed PR.
    ///
    /// The fix: `all_claimed_execution_ids()` unions all three pools. This
    /// test verifies the fix by claiming the pr_review execution in the
    /// review pool only (never the main pool) and asserting the sweep does
    /// not abandon it.
    #[tokio::test]
    async fn running_pr_review_in_review_pool_is_not_abandoned() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        make_old(&db, &work_item_id);

        // Create a pr_review execution and force it to `running` to simulate
        // a reviewer pane that was successfully spawned.
        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .build(),
            )
            .unwrap();
        // Override kind to PrReview — the execution was created with the
        // default kind; we force the DB value directly so the sweep reads it.
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET kind = 'pr_review', status = 'running' WHERE id = ?1",
                rusqlite::params![execution.id],
            )
            .unwrap();
        }

        let db = Arc::new(db);
        // Build a coordinator with a 1-slot main pool AND a 1-slot review pool.
        // Claim the pr_review execution in the REVIEW pool (not the main pool)
        // to simulate the production layout: main pool has an idle slot (so
        // the fast-path check passes), but the reviewer is live in review pool.
        let (coordinator, review_pool) = make_coordinator_with_review_pool(db.clone(), 1, 1);
        review_pool.claim_worker(&execution.id, None).await;
        // Main pool is idle — this is what previously triggered the bug:
        // has_idle_worker() = true (sweep proceeds), but the main-pool
        // claimed_execution_ids() didn't include the reviewer exec id.

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref()).await;

        assert_eq!(
            outcome.redispatched, 0,
            "sweep must not re-dispatch when the pr_review execution is claimed in the review pool"
        );
        assert_eq!(
            outcome.running_reviewer_skipped, 0,
            "defense-in-depth skip must not fire when pool union correctly identifies the reviewer as live"
        );
        let events = sink.events().await;
        assert!(
            events.iter().all(|e| e.stage != "orphan_active_redispatch"),
            "no orphan_active_redispatch event must fire for a live review-pool-claimed reviewer"
        );

        // The running pr_review execution must remain intact — not abandoned.
        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            executions
                .iter()
                .any(|e| e.id == execution.id && e.status == ExecutionStatus::Running),
            "running pr_review execution must not be abandoned by the sweep"
        );
    }

    /// Defense-in-depth regression: even if the pool-union fix were somehow
    /// absent (e.g. a future refactor splits pools again), the explicit
    /// `running pr_review` guard in `run_one_pass` must fire and prevent
    /// abandoning the live reviewer.
    #[tokio::test]
    async fn running_pr_review_not_in_any_pool_hits_defense_in_depth_skip() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        make_old(&db, &work_item_id);

        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .build(),
            )
            .unwrap();
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET kind = 'pr_review', status = 'running' WHERE id = ?1",
                rusqlite::params![execution.id],
            )
            .unwrap();
        }

        let db = Arc::new(db);
        // Claim nothing in any pool — simulates the "pool union absent" scenario.
        let coordinator = make_coordinator(db.clone(), 1);

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(db.as_ref(), coordinator.clone(), sink.as_ref()).await;

        assert_eq!(
            outcome.redispatched, 0,
            "defense-in-depth guard must prevent re-dispatch of a running pr_review execution"
        );
        assert_eq!(
            outcome.running_reviewer_skipped, 1,
            "defense-in-depth skip counter must be incremented"
        );
        let executions = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            executions
                .iter()
                .any(|e| e.id == execution.id && e.status == ExecutionStatus::Running),
            "running pr_review execution must survive the sweep even when not in any pool"
        );
    }
}
