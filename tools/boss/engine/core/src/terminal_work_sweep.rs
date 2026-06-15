//! Periodic reconciler that reaps a LIVE worker pane whose bound work
//! item (or its execution) has already reached a terminal state.
//!
//! ## The zombie this closes (the "O'Brien" case)
//!
//! A worker's normal terminal act is opening its PR; the engine reaps it
//! immediately afterward via the completion path's
//! [`crate::app::ServerState::release_worker_pane`]. But that teardown can
//! fail to land: the laptop is closed, an API call wedges, the run hangs
//! mid-turn. The worker then sits alive — typically in `waiting_for_input`
//! — holding its slot indefinitely, long after its task went `done` and
//! its PR merged. The real incident: run `exec_…8d` on slot 8 stayed alive
//! for ~2.5 DAYS after work item T1679 had gone to `done` and PR #1496 had
//! MERGED, and had to be reaped by hand.
//!
//! Every other reconciler deliberately skips this case:
//!
//! * [`crate::dead_pid_sweep`] requires a *dead* PID AND a non-terminal
//!   status — the O'Brien worker's PID is alive and its status may be
//!   terminal, so it is skipped on both counts.
//! * [`crate::stale_worker_sweep`] only inspects slots whose activity is
//!   `working` with no tool in flight — a `waiting_for_input` zombie is
//!   skipped.
//! * [`crate::transient_recovery`] recovers *unfinished* work after a
//!   transient API error; it never checks whether the work item is already
//!   done, and respawning a `done` task is meaningless.
//! * [`crate::pool_claim_sweep`] reconciles the engine's *pool claim*, and
//!   deliberately leaves any claim still backed by a live pane to the
//!   completion path — exactly the path that failed to fire here.
//!
//! So nothing reaps a live worker whose reason for existing is already
//! gone. This sweep closes that gap.
//!
//! ## Why this is the safest possible reap signal
//!
//! The operator's primary constraint is: never reap a worker that is
//! actively doing things. This sweep reaps ONLY on strong, positive
//! evidence of terminality — the bound work item is `done` / `archived` /
//! `cancelled`, or the execution itself is `completed` / `failed` /
//! `cancelled` / `orphaned`. A `done` task means the PR already merged: the
//! work provably landed, so there is nothing active to destroy. An
//! `in_review` task (PR open, awaiting review) is NOT terminal and is left
//! alone. A worker bound to a *revision* of a done task carries the
//! revision's (still-active) work-item id, not the parent's, so it is never
//! mistaken for the zombie.
//!
//! Two independent guards make a wrong reap impossible:
//!
//! 1. **Two-pass confirmation.** A candidate is reaped only if it was ALSO
//!    a terminal-and-live candidate on the immediately preceding pass — so
//!    it must have been terminal+live for at least one full [`DEFAULT_INTERVAL`].
//!    Normal completion teardown fires within seconds of terminality, so a
//!    candidate that survives a full interval is a genuine strand, never a
//!    teardown still in flight. If the status flips back to non-terminal
//!    between passes (a reopened task), the candidate drops out and is not
//!    reaped.
//! 2. **Run-id-keyed idempotent reap.** Reaping goes through the same
//!    `release_worker_pane(run_id)` the completion path uses, which resolves
//!    the slot from the run id via an atomic `take_slot_for_run`. If the
//!    slot was freed and recycled to a DIFFERENT execution between this
//!    sweep's snapshot and the reap call, the lookup returns `None` and the
//!    reap is a no-op — we can never tear down a live worker that merely
//!    happens to occupy the same slot now.
//!
//! When in doubt the sweep does nothing: a DB lookup failure is a
//! conservative skip, and a non-terminal worker is simply left running.
//!
//! ## Cadence
//!
//! Runs every [`DEFAULT_INTERVAL`] and fires once shortly after boot. The
//! first pass after boot only *records* candidates (two-pass guard), so the
//! earliest a post-restart zombie is reaped is one interval in — giving any
//! legitimate teardown a full interval to act first.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::work::WorkDb;

/// How often the terminal-work reconciler runs. 60s mirrors the dead-pid,
/// stale-worker, and pool-claim sweeps. Because reaping requires a
/// candidate to persist across two consecutive passes, this interval also
/// sets the minimum confirmation delay before a strand is reaped.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// Tears down a live worker pane bound to a run id. Implemented by
/// [`crate::app::ServerState`] (delegating to `release_worker_pane`);
/// stubbed in tests and in contexts without an app session.
#[async_trait::async_trait]
pub trait WorkerReaper: Send + Sync {
    /// Reap the live worker pane bound to `run_id` (the execution id):
    /// release the app pane, signal the OS process tree, free the engine
    /// pool slot, and drop the live-state entry — the same teardown the
    /// completion path uses.
    ///
    /// MUST be idempotent and keyed on `run_id`: if the slot mapped to
    /// `run_id` has already been released (or recycled to a different
    /// execution) the call MUST be a no-op. The reconciler relies on this
    /// to guarantee it never reaps the wrong (live) worker.
    async fn reap_terminal_worker(&self, run_id: &str);
}

/// No-op reaper for contexts without an app session (and a default for
/// tests that don't care about the teardown side effect).
pub struct NoopWorkerReaper;

#[async_trait::async_trait]
impl WorkerReaper for NoopWorkerReaper {
    async fn reap_terminal_worker(&self, _run_id: &str) {}
}

/// Counts from one sweep pass; logged at `info` when any worker was reaped.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TerminalWorkSweepOutcome {
    /// Stranded workers (terminal work item / execution, confirmed across
    /// two passes) that were reaped this pass.
    pub reaped: usize,
    /// Candidates seen terminal+live for the first time; held for one more
    /// pass before any reap (two-pass confirmation).
    pub pending_confirmation: usize,
    /// Live workers left alone because their work item and execution are
    /// both still non-terminal — the normal, healthy case.
    pub active_skipped: usize,
    /// Slots skipped this pass because the execution lookup failed
    /// (conservative — retried next pass).
    pub lookup_failed_skipped: usize,
}

impl TerminalWorkSweepOutcome {
    fn has_activity(&self) -> bool {
        self.reaped > 0
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`,
/// threading the cross-pass candidate set so the two-pass confirmation
/// guard survives between passes.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    live_states: Arc<LiveWorkerStateRegistry>,
    reaper: Arc<dyn WorkerReaper>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Candidates observed terminal+live on the previous pass. A
        // candidate is reaped only once it appears in two consecutive
        // passes, so this set is the confirmation memory.
        let mut seen_terminal: HashSet<String> = HashSet::new();
        loop {
            let outcome = run_one_pass(
                work_db.as_ref(),
                live_states.as_ref(),
                reaper.as_ref(),
                dispatch_events.as_ref(),
                &mut seen_terminal,
            )
            .await;
            if outcome.has_activity() {
                tracing::info!(
                    reaped = outcome.reaped,
                    pending_confirmation = outcome.pending_confirmation,
                    active_skipped = outcome.active_skipped,
                    lookup_failed_skipped = outcome.lookup_failed_skipped,
                    "terminal-work sweep: reaped stranded worker(s) whose work was already terminal",
                );
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// Run a single terminal-work reconciliation pass over every live worker
/// slot. `seen_terminal` carries the set of run ids observed
/// terminal-and-live on the *previous* pass; on return it holds this pass's
/// candidates so the next pass can confirm them. Returns a summary; callers
/// may log it.
pub async fn run_one_pass(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    reaper: &dyn WorkerReaper,
    dispatch_events: &dyn DispatchEventSink,
    seen_terminal: &mut HashSet<String>,
) -> TerminalWorkSweepOutcome {
    let mut outcome = TerminalWorkSweepOutcome::default();
    // Candidates observed this pass — becomes `seen_terminal` for the next.
    let mut current_candidates: HashSet<String> = HashSet::new();

    for state in live_states.snapshot() {
        // `run_id` on a live-state entry IS the execution id (see
        // dead_pid_sweep / pool_claim_sweep).
        let run_id = state.run_id;

        // Look up the execution: it gives us both the execution's own
        // terminality and the bound work-item id. A DB error is not proof
        // of anything — skip conservatively and retry next pass.
        let execution = match work_db.get_execution(&run_id) {
            Ok(execution) => execution,
            Err(err) => {
                tracing::warn!(
                    run_id = %run_id,
                    slot_id = state.slot_id,
                    ?err,
                    "terminal-work sweep: failed to look up execution; skipping this pass",
                );
                outcome.lookup_failed_skipped += 1;
                continue;
            }
        };

        let execution_terminal = execution.status.is_terminal();

        // The O'Brien signal: the bound work item is terminal (done /
        // archived / cancelled) even though the worker — and possibly its
        // execution — is still alive. A work-item lookup failure falls back
        // to the execution signal alone rather than guessing.
        let work_item_terminal = match work_db.get_work_item(&execution.work_item_id) {
            Ok(item) => work_item_is_terminal(&item),
            Err(err) => {
                tracing::warn!(
                    run_id = %run_id,
                    work_item_id = %execution.work_item_id,
                    ?err,
                    "terminal-work sweep: failed to look up bound work item; using execution status only",
                );
                false
            }
        };

        if !work_item_terminal && !execution_terminal {
            // Healthy: the worker still has live work to do. Leave it be.
            outcome.active_skipped += 1;
            continue;
        }

        let reason = if work_item_terminal {
            "work_item_terminal"
        } else {
            "execution_terminal"
        };

        // This slot is a reap candidate this pass.
        current_candidates.insert(run_id.clone());

        // Two-pass confirmation: only reap if this run was ALSO a
        // terminal+live candidate on the previous pass. A teardown still in
        // flight would have completed within the interval, so survival
        // across a full interval marks a genuine strand.
        if !seen_terminal.contains(&run_id) {
            outcome.pending_confirmation += 1;
            tracing::debug!(
                run_id = %run_id,
                slot_id = state.slot_id,
                reason,
                "terminal-work sweep: terminal worker observed; awaiting next-pass confirmation before reaping",
            );
            continue;
        }

        tracing::warn!(
            run_id = %run_id,
            slot_id = state.slot_id,
            work_item_id = %execution.work_item_id,
            execution_status = %execution.status,
            reason,
            "terminal-work sweep: live worker pane outlived its terminal work; reaping and freeing slot",
        );

        // Reap via the canonical, idempotent, run-id-keyed teardown. If the
        // slot was recycled to a different execution since the snapshot,
        // this is a no-op and the live worker now in the slot is untouched.
        reaper.reap_terminal_worker(&run_id).await;
        outcome.reaped += 1;

        dispatch_events
            .emit(
                DispatchEvent::new(Stage::TerminalWorkReconcile, Outcome::Ok, &run_id)
                    .with_work_item(&execution.work_item_id)
                    .with_worker(&crate::coordinator::worker_id_for_slot(state.slot_id))
                    .with_details(serde_json::json!({
                        "reason": reason,
                        "slot_id": state.slot_id,
                        "execution_status": execution.status,
                        "work_item_terminal": work_item_terminal,
                        "execution_terminal": execution_terminal,
                    })),
            )
            .await;
    }

    *seen_terminal = current_candidates;
    outcome
}

/// Whether a bound work item is in a terminal status. Only task-shaped
/// work items (tasks and chores) are dispatched to workers; product /
/// project bindings are treated as non-terminal so the sweep never reaps on
/// a binding shape it does not model.
fn work_item_is_terminal(item: &boss_protocol::WorkItem) -> bool {
    use boss_protocol::WorkItem;
    match item {
        WorkItem::Task(task) | WorkItem::Chore(task) => task.status.is_terminal(),
        WorkItem::Product(_) | WorkItem::Project(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use boss_protocol::WorkItemBinding;
    use tempfile::TempDir;

    use super::*;
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::live_worker_state::LiveWorkerStateRegistry;
    use crate::work::{CreateChoreInput, CreateProductInput, WorkDb, WorkItemPatch};

    // ─── reaper stub ─────────────────────────────────────────────────────────

    /// Records every `run_id` it was asked to reap, and (to model a real
    /// teardown) drops the matching live-state entry so subsequent passes no
    /// longer see the slot. `tear_down = false` models a reap that did not
    /// land (app unreachable) — the entry stays, exercising retry behaviour.
    struct RecordingReaper {
        live_states: Arc<LiveWorkerStateRegistry>,
        reaped: Mutex<Vec<String>>,
        tear_down: bool,
    }

    impl RecordingReaper {
        fn new(live_states: Arc<LiveWorkerStateRegistry>, tear_down: bool) -> Self {
            Self {
                live_states,
                reaped: Mutex::new(Vec::new()),
                tear_down,
            }
        }
        fn reaped(&self) -> Vec<String> {
            self.reaped.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl WorkerReaper for RecordingReaper {
        async fn reap_terminal_worker(&self, run_id: &str) {
            self.reaped.lock().unwrap().push(run_id.to_owned());
            if self.tear_down
                && let Some(slot) = self
                    .live_states
                    .snapshot()
                    .into_iter()
                    .find(|s| s.run_id == run_id)
                    .map(|s| s.slot_id)
            {
                self.live_states.release_slot(slot);
            }
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

    /// Create a chore in `active` status (the normal dispatched state).
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
        set_work_item_status(db, &chore.id, "active");
        chore.id
    }

    fn set_work_item_status(db: &WorkDb, work_item_id: &str, status: &str) {
        db.update_work_item(
            work_item_id,
            WorkItemPatch {
                status: Some(status.to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
    }

    fn create_execution(db: &WorkDb, work_item_id: &str) -> String {
        use boss_protocol::RequestExecutionInput;
        db.request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
            .unwrap()
            .id
    }

    /// Raw UPDATE to drive an execution to a terminal status without a full
    /// running-run setup.
    fn force_execution_status(db: &WorkDb, execution_id: &str, status: &str) {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET status = ?2 WHERE id = ?1",
            rusqlite::params![execution_id, status],
        )
        .unwrap();
    }

    /// Register a live worker pane (activity `Spawning`, an alive PID) bound
    /// to `work_item_id` / `execution_id`.
    fn register_live_worker(
        live_states: &LiveWorkerStateRegistry,
        slot_id: u8,
        execution_id: &str,
        work_item_id: &str,
    ) {
        live_states.register_spawn(
            slot_id,
            execution_id,
            "claude-opus-4-8",
            std::process::id() as i32,
            Some(WorkItemBinding {
                work_item_id: work_item_id.to_owned(),
                work_item_name: "test chore".to_owned(),
                execution_id: execution_id.to_owned(),
            }),
        );
    }

    fn setup() -> (TempDir, Arc<WorkDb>, String) {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        (dir, Arc::new(db), product_id)
    }

    // ─── tests ───────────────────────────────────────────────────────────────

    /// The O'Brien regression: a live worker whose WORK ITEM is `done` (its
    /// execution still non-terminal — hung) is reaped, but only after the
    /// two-pass confirmation. The first pass records it; the second reaps.
    #[tokio::test]
    async fn reaps_worker_whose_work_item_is_done() {
        let (_dir, db, product_id) = setup();
        let work_item_id = create_active_chore(&db, &product_id, "obrien");
        let execution_id = create_execution(&db, &work_item_id);

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_live_worker(&live_states, 8, &execution_id, &work_item_id);

        // Work item goes done (PR merged); execution left non-terminal,
        // modelling the wedged worker.
        set_work_item_status(&db, &work_item_id, "done");

        let reaper = RecordingReaper::new(live_states.clone(), true);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let mut seen = HashSet::new();

        // Pass 1: candidate observed, not yet reaped.
        let first = run_one_pass(db.as_ref(), &live_states, &reaper, sink.as_ref(), &mut seen).await;
        assert_eq!(first.reaped, 0, "first pass must only record the candidate");
        assert_eq!(first.pending_confirmation, 1);
        assert!(reaper.reaped().is_empty());
        assert!(sink.events().await.is_empty());

        // Pass 2: candidate confirmed across two passes — reap.
        let second = run_one_pass(db.as_ref(), &live_states, &reaper, sink.as_ref(), &mut seen).await;
        assert_eq!(second.reaped, 1, "second pass must reap the confirmed strand");
        assert_eq!(reaper.reaped(), vec![execution_id.clone()]);

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "terminal_work_reconcile");
        assert_eq!(events[0].outcome, "ok");
        assert_eq!(events[0].work_item_id.as_deref(), Some(work_item_id.as_str()));
        assert_eq!(events[0].details["reason"], "work_item_terminal");
        assert_eq!(events[0].details["slot_id"], 8);

        // Pass 3: the live state was torn down, so nothing remains to reap.
        let third = run_one_pass(db.as_ref(), &live_states, &reaper, sink.as_ref(), &mut seen).await;
        assert_eq!(third.reaped, 0);
        assert_eq!(third.pending_confirmation, 0);
        assert_eq!(third.active_skipped, 0);
    }

    /// A live worker whose EXECUTION is terminal (completion ran but the
    /// pane teardown never landed) is reaped after confirmation, even though
    /// its work item is not itself terminal.
    #[tokio::test]
    async fn reaps_worker_whose_execution_is_terminal() {
        let (_dir, db, product_id) = setup();
        let work_item_id = create_active_chore(&db, &product_id, "exec-terminal");
        let execution_id = create_execution(&db, &work_item_id);

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_live_worker(&live_states, 3, &execution_id, &work_item_id);
        force_execution_status(&db, &execution_id, "completed");

        let reaper = RecordingReaper::new(live_states.clone(), true);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let mut seen = HashSet::new();

        run_one_pass(db.as_ref(), &live_states, &reaper, sink.as_ref(), &mut seen).await;
        let second = run_one_pass(db.as_ref(), &live_states, &reaper, sink.as_ref(), &mut seen).await;

        assert_eq!(second.reaped, 1);
        assert_eq!(reaper.reaped(), vec![execution_id]);
        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].details["reason"], "execution_terminal");
    }

    /// The safety core: a live worker whose work item is `active` and whose
    /// execution is non-terminal is NEVER reaped, no matter how many passes
    /// run. This is the "do not reap an active worker" invariant.
    #[tokio::test]
    async fn never_reaps_active_worker() {
        let (_dir, db, product_id) = setup();
        let work_item_id = create_active_chore(&db, &product_id, "busy");
        let execution_id = create_execution(&db, &work_item_id);

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_live_worker(&live_states, 2, &execution_id, &work_item_id);

        let reaper = RecordingReaper::new(live_states.clone(), true);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let mut seen = HashSet::new();

        for _ in 0..5 {
            let outcome = run_one_pass(db.as_ref(), &live_states, &reaper, sink.as_ref(), &mut seen).await;
            assert_eq!(outcome.reaped, 0, "active worker must never be reaped");
            assert_eq!(outcome.active_skipped, 1);
        }
        assert!(reaper.reaped().is_empty());
        assert!(sink.events().await.is_empty());
    }

    /// A worker that is terminal on pass 1 but whose status flips back to
    /// active before pass 2 (a reopened task) is NOT reaped — the two-pass
    /// guard re-checks current terminality each pass rather than trusting
    /// stale memory.
    #[tokio::test]
    async fn does_not_reap_when_status_reverts_before_confirmation() {
        let (_dir, db, product_id) = setup();
        let work_item_id = create_active_chore(&db, &product_id, "flapper");
        let execution_id = create_execution(&db, &work_item_id);

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_live_worker(&live_states, 5, &execution_id, &work_item_id);
        set_work_item_status(&db, &work_item_id, "done");

        let reaper = RecordingReaper::new(live_states.clone(), true);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let mut seen = HashSet::new();

        // Pass 1: candidate recorded.
        let first = run_one_pass(db.as_ref(), &live_states, &reaper, sink.as_ref(), &mut seen).await;
        assert_eq!(first.pending_confirmation, 1);

        // Status reverts to active before the confirming pass.
        set_work_item_status(&db, &work_item_id, "active");

        let second = run_one_pass(db.as_ref(), &live_states, &reaper, sink.as_ref(), &mut seen).await;
        assert_eq!(second.reaped, 0, "reverted candidate must not be reaped");
        assert_eq!(second.active_skipped, 1);
        assert!(reaper.reaped().is_empty());
    }

    /// A DB lookup failure (execution row missing) is a conservative skip,
    /// never a reap.
    #[tokio::test]
    async fn lookup_failure_is_conservative_skip() {
        let (_dir, db, _product_id) = setup();

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        // Live state references an execution id that does not exist in the DB.
        register_live_worker(&live_states, 1, "exec-does-not-exist", "wi-missing");

        let reaper = RecordingReaper::new(live_states.clone(), true);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let mut seen = HashSet::new();

        for _ in 0..3 {
            let outcome = run_one_pass(db.as_ref(), &live_states, &reaper, sink.as_ref(), &mut seen).await;
            assert_eq!(outcome.reaped, 0);
            assert_eq!(outcome.lookup_failed_skipped, 1);
        }
        assert!(reaper.reaped().is_empty());
    }

    /// If a reap does not land (app unreachable — the live state stays), the
    /// strand remains a confirmed candidate and is reaped again on the next
    /// pass. Persistent retry is the desired behaviour for the O'Brien hang.
    #[tokio::test]
    async fn retries_reap_when_teardown_does_not_land() {
        let (_dir, db, product_id) = setup();
        let work_item_id = create_active_chore(&db, &product_id, "wedged");
        let execution_id = create_execution(&db, &work_item_id);

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_live_worker(&live_states, 7, &execution_id, &work_item_id);
        set_work_item_status(&db, &work_item_id, "cancelled");

        // tear_down = false: the reap is recorded but the live state is left
        // in place, modelling an app that did not actually release the pane.
        let reaper = RecordingReaper::new(live_states.clone(), false);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let mut seen = HashSet::new();

        run_one_pass(db.as_ref(), &live_states, &reaper, sink.as_ref(), &mut seen).await; // record
        let p2 = run_one_pass(db.as_ref(), &live_states, &reaper, sink.as_ref(), &mut seen).await; // reap
        let p3 = run_one_pass(db.as_ref(), &live_states, &reaper, sink.as_ref(), &mut seen).await; // retry

        assert_eq!(p2.reaped, 1);
        assert_eq!(p3.reaped, 1, "an un-landed reap must be retried next pass");
        assert_eq!(reaper.reaped(), vec![execution_id.clone(), execution_id]);
    }
}
