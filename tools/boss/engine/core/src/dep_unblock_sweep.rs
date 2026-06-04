//! Periodic safety-net sweeper for dependency-blocked work items.
//!
//! ## Why this exists
//!
//! The primary unblock path is event-driven: when a prerequisite
//! transitions to a satisfied status (`done` / `archived`), the DB
//! transaction that writes that transition immediately calls
//! `cascade_dependents_after_prereq_status_change`, which walks every
//! `blocks` dependent and calls `maybe_engine_unblock_dependent` for
//! each.
//!
//! That path is fast (sub-second), but it has one known silent-skip
//! condition: if a concurrent update to the dependent row changed
//! `last_status_actor` to `'human'` between when the engine auto-blocked
//! it and when the prereq landed, the old cascade guard would skip the
//! row silently and leave it wedged.
//!
//! The guard was widened (PR that ships this module) to use
//! `blocked_reason = 'dependency'` as the primary signal, making the
//! cascade much more robust. This sweeper is an additional safety net:
//! it re-evaluates every dependency-blocked task on a ≤60 s cadence so
//! that any item the event path still misses (e.g. engine offline at
//! prereq transition time, or a future guard regression) is recovered
//! within one sweep interval rather than hours.
//!
//! ## Observed incident (2026-05-13)
//!
//! T335 (`task_18af2d6114d16c70_26`, "Bazel installer rule + payload
//! assembly") was blocked on T343 (`task_18af317331d4b960_1`, "Fix
//! critical scoping bug: boss uninstall …"). T343 transitioned to
//! `status=done` at 2026-05-13 19:33:09 Z (PR #424). T335 remained
//! `status=blocked` for roughly three hours until ~23:18 Z, when it
//! flipped to `todo` spontaneously via an unrelated request-execution
//! path that happened to clear the stale block. Without this sweeper
//! the only recovery path was manual operator action or an accidental
//! trigger via `RequestExecution`.
//!
//! ## Cadence
//!
//! Runs every [`DEP_UNBLOCK_SWEEP_INTERVAL_SECS`] seconds (default 30)
//! and fires immediately on spawn. The sweep is cheap: one DB read of
//! `tasks WHERE status='blocked'` (typically a small set) followed by
//! per-row prereq status lookups.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::metrics::Registry;
use crate::work::WorkDb;
#[cfg(test)]
use crate::work::TaskStatus;

/// Interval between sweep passes.
pub const DEP_UNBLOCK_SWEEP_INTERVAL_SECS: u64 = 30;

crate::register_gauge!(
    DEP_UNBLOCK_LONGEST_STALE_SECONDS,
    "dependency_unblock.longest_stale_seconds",
    "Seconds since updated_at for the longest-stale dependency-blocked row observed in the most recent sweep.",
);

/// Register all dep-unblock gauge handles with `registry`. Called from
/// [`crate::metrics::init_all`] at engine startup.
pub fn register_metrics(registry: &Registry) {
    registry.register_gauge(&DEP_UNBLOCK_LONGEST_STALE_SECONDS);
}

/// Counters from one sweep pass.
#[derive(Debug, Default)]
pub struct DepUnblockSweepOutcome {
    /// Number of dependency-blocked tasks evaluated this pass.
    pub rows_evaluated: usize,
    /// Number of tasks that were unblocked (all prereqs now satisfied).
    pub rows_unblocked: usize,
    /// Seconds since `updated_at` for the longest-stale evaluated row.
    /// Zero when `rows_evaluated == 0`.
    pub longest_stale_secs: u64,
    /// Number of `todo, autostart=true` tasks whose stuck
    /// `waiting_dependency` execution was promoted to `ready` (Part B
    /// recovery sweep).
    pub rows_stuck_promoted: usize,
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn so items blocked before engine boot are
/// recovered without waiting for the first interval.
///
/// `kick_fn` is called whenever the sweep does any work (unblocks or
/// promotes a stuck execution) so the coordinator scheduler is woken
/// and picks up the newly-ready executions immediately.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    interval: Duration,
    metrics: Arc<Registry>,
    kick_fn: Arc<dyn Fn() + Send + Sync>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let outcome = run_one_pass(work_db.as_ref()).await;
            DEP_UNBLOCK_LONGEST_STALE_SECONDS.set(&metrics, outcome.longest_stale_secs as i64);
            tracing::info!(
                rows_evaluated = outcome.rows_evaluated,
                rows_unblocked = outcome.rows_unblocked,
                rows_stuck_promoted = outcome.rows_stuck_promoted,
                longest_stale_secs = outcome.longest_stale_secs,
                "dep-unblock sweep: pass complete",
            );
            if outcome.rows_unblocked > 0 || outcome.rows_stuck_promoted > 0 {
                kick_fn();
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// Run a single sweep pass. Returns per-pass counters for the caller to
/// log or assert in tests.
pub async fn run_one_pass(work_db: &WorkDb) -> DepUnblockSweepOutcome {
    let mut outcome = DepUnblockSweepOutcome::default();

    let candidates = match work_db.list_dependency_blocked_candidates() {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(?err, "dep-unblock sweep: failed to list candidates; skipping pass");
            return outcome;
        }
    };

    outcome.rows_evaluated = candidates.len();

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    for (work_item_id, updated_at_epoch) in candidates {
        let stale_secs = now_secs.saturating_sub(updated_at_epoch as u64);
        if stale_secs > outcome.longest_stale_secs {
            outcome.longest_stale_secs = stale_secs;
        }

        match work_db.try_unblock_dependency_if_resolved(&work_item_id) {
            Ok(true) => {
                tracing::info!(
                    work_item_id = %work_item_id,
                    stale_secs,
                    "dep-unblock sweep: unblocked stale dependent — all gating prereqs satisfied",
                );
                outcome.rows_unblocked += 1;
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(
                    work_item_id = %work_item_id,
                    ?err,
                    "dep-unblock sweep: failed to check/unblock candidate; skipping",
                );
            }
        }
    }

    // Part B recovery: find `todo, autostart=true` tasks stuck with a
    // `waiting_dependency` execution (or no execution at all) despite
    // having no gating prereqs, and promote their execution to `ready`.
    // This recovers tasks that were auto-unblocked before the event-path
    // fix landed — their status is already `todo` but the execution was
    // never promoted, so the coordinator never saw them.
    match work_db.promote_todo_autostart_stuck_executions() {
        Ok(promoted) if !promoted.is_empty() => {
            outcome.rows_stuck_promoted = promoted.len();
        }
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(?err, "dep-unblock sweep: failed to promote stuck executions; skipping");
        }
    }

    outcome
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;
    use crate::work::{
        AddDependencyInput, CreateChoreInput, CreateProductInput, ExecutionStatus, WorkDb,
        WorkItemPatch,
    };

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

    fn create_chore(db: &WorkDb, product_id: &str, name: &str) -> String {
        db.create_chore(CreateChoreInput {
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
        .unwrap()
        .id
    }

    fn create_chore_no_autostart(db: &WorkDb, product_id: &str, name: &str) -> String {
        db.create_chore(CreateChoreInput {
            product_id: product_id.to_owned(),
            name: name.to_owned(),
            description: None,
            repo_remote_url: None,
            priority: None,
            effort_level: None,
            model_override: None,
            created_via: None,
            autostart: false,
            force_duplicate: false,
        })
        .unwrap()
        .id
    }

    /// Safety-net path: prereq goes to `done` via a path that bypasses the
    /// event-driven cascade (simulating engine-offline or a future cascade
    /// regression). The sweep must detect and unblock the dependent within
    /// one pass.
    #[tokio::test]
    async fn sweep_unblocks_dependent_when_prereq_done_without_cascade() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let prereq_id = create_chore(&db, &product_id, "prereq");
        let dep_id = create_chore(&db, &product_id, "dependent");

        // Wire dependency — auto-blocks the dependent.
        db.add_dependency(AddDependencyInput {
            dependent: dep_id.clone(),
            prerequisite: prereq_id.clone(),
            relation: None,
        })
        .unwrap();

        // Verify auto-block.
        let dep_before = db.get_work_item(&dep_id).unwrap();
        let boss_protocol::WorkItem::Chore(before) = dep_before else { panic!() };
        assert_eq!(before.status, TaskStatus::Blocked);
        assert_eq!(before.blocked_reason.as_deref(), Some("dependency"));

        // Mark prereq done without triggering the cascade.
        db.mark_task_done_for_test_no_cascade(&prereq_id).unwrap();

        // Dependent still blocked — cascade didn't fire.
        let dep_still = db.get_work_item(&dep_id).unwrap();
        let boss_protocol::WorkItem::Chore(still) = dep_still else { panic!() };
        assert_eq!(still.status, TaskStatus::Blocked, "must still be blocked before sweep");

        // Sweep must recover it.
        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;

        assert_eq!(outcome.rows_evaluated, 1);
        assert_eq!(outcome.rows_unblocked, 1, "sweep must unblock the stale dependent");

        let dep_after = db.get_work_item(&dep_id).unwrap();
        let boss_protocol::WorkItem::Chore(after) = dep_after else { panic!() };
        assert_eq!(after.status, TaskStatus::Todo);
        assert!(after.blocked_reason.is_none(), "blocked_reason must be cleared");
    }

    /// Actor-mismatch path: the engine auto-blocked the dependent, a
    /// subsequent update reset `last_status_actor` to `'human'`. The sweep
    /// must still recover it because `blocked_reason = 'dependency'` is the
    /// authoritative signal (the 2026-05-13 incident scenario).
    #[tokio::test]
    async fn sweep_unblocks_when_actor_is_human_but_blocked_reason_is_dependency() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let prereq_id = create_chore(&db, &product_id, "prereq");
        let dep_id = create_chore(&db, &product_id, "dependent");

        db.add_dependency(AddDependencyInput {
            dependent: dep_id.clone(),
            prerequisite: prereq_id.clone(),
            relation: None,
        })
        .unwrap();

        // Simulate a concurrent update that reset last_status_actor to 'human'.
        db.force_last_status_actor_for_test(&dep_id, "human").unwrap();

        // Mark prereq done without cascade.
        db.mark_task_done_for_test_no_cascade(&prereq_id).unwrap();

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;

        assert_eq!(outcome.rows_unblocked, 1, "sweep must unblock despite last_status_actor='human'");

        let dep_after = db.get_work_item(&dep_id).unwrap();
        let boss_protocol::WorkItem::Chore(after) = dep_after else { panic!() };
        assert_eq!(after.status, TaskStatus::Todo);
        assert!(after.blocked_reason.is_none());
    }

    /// The cascade fix (using blocked_reason as primary guard) must also
    /// unblock the dependent directly when the prereq goes to done via the
    /// normal update path — even if last_status_actor was reset to 'human'.
    #[tokio::test]
    async fn cascade_unblocks_via_update_work_item_despite_human_actor() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let prereq_id = create_chore(&db, &product_id, "prereq");
        let dep_id = create_chore(&db, &product_id, "dependent");

        db.add_dependency(AddDependencyInput {
            dependent: dep_id.clone(),
            prerequisite: prereq_id.clone(),
            relation: None,
        })
        .unwrap();

        // Reset actor to 'human' (previously caused cascade to skip this row).
        db.force_last_status_actor_for_test(&dep_id, "human").unwrap();

        // Mark prereq done via the normal path — cascade fires.
        db.update_work_item(
            &prereq_id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        // With the new blocked_reason guard the cascade must now unblock directly.
        let dep_after = db.get_work_item(&dep_id).unwrap();
        let boss_protocol::WorkItem::Chore(after) = dep_after else { panic!() };
        assert_eq!(
            after.status,
            TaskStatus::Todo,
            "cascade must unblock when blocked_reason='dependency', even if actor='human'",
        );
        assert!(after.blocked_reason.is_none());
    }

    /// A dependent with a still-unsatisfied prereq must not be unblocked.
    #[tokio::test]
    async fn sweep_leaves_dependent_blocked_when_prereq_still_active() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let prereq_id = create_chore(&db, &product_id, "prereq");
        let dep_id = create_chore(&db, &product_id, "dependent");

        db.add_dependency(AddDependencyInput {
            dependent: dep_id.clone(),
            prerequisite: prereq_id.clone(),
            relation: None,
        })
        .unwrap();

        // prereq is still 'todo' — no unblock should happen.
        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;

        assert_eq!(outcome.rows_evaluated, 1);
        assert_eq!(outcome.rows_unblocked, 0, "must not unblock when prereq is still todo");

        let dep_after = db.get_work_item(&dep_id).unwrap();
        let boss_protocol::WorkItem::Chore(after) = dep_after else { panic!() };
        assert_eq!(after.status, TaskStatus::Blocked);
    }

    /// A manually-blocked item (blocked_reason IS NULL, last_status_actor = 'human')
    /// must not appear in the sweeper's candidate list.
    #[tokio::test]
    async fn sweep_ignores_human_blocked_items_without_dependency_reason() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let task_id = create_chore(&db, &product_id, "manually-blocked");

        // Block manually via the public API — last_status_actor='human', blocked_reason=NULL.
        db.update_work_item(
            &task_id,
            WorkItemPatch {
                status: Some("blocked".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;

        assert_eq!(
            outcome.rows_evaluated, 0,
            "human-blocked items must not appear in candidates",
        );
        assert_eq!(outcome.rows_unblocked, 0);
    }

    /// When a task is auto-unblocked via the sweep, `rows_unblocked` is set so the
    /// caller (spawn_loop) knows to fire kick_fn.
    #[tokio::test]
    async fn sweep_reports_unblock_in_outcome() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let prereq_id = create_chore(&db, &product_id, "prereq");
        let dep_id = create_chore(&db, &product_id, "dependent");

        db.add_dependency(AddDependencyInput {
            dependent: dep_id.clone(),
            prerequisite: prereq_id.clone(),
            relation: None,
        })
        .unwrap();

        db.mark_task_done_for_test_no_cascade(&prereq_id).unwrap();

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;

        assert_eq!(outcome.rows_unblocked, 1, "outcome must record the unblock so spawn_loop can kick");
    }

    /// Part B recovery: a `todo, autostart=true` task with a `waiting_dependency`
    /// execution and no gating prereqs must be promoted to `ready` by the sweep.
    /// This covers the T664 regression where auto-unblock wrote `todo` but the
    /// execution was never promoted.
    #[tokio::test]
    async fn sweep_promotes_stuck_todo_waiting_dependency_execution() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let prereq_id = create_chore(&db, &product_id, "prereq");
        let dep_id = create_chore(&db, &product_id, "dependent");

        db.add_dependency(AddDependencyInput {
            dependent: dep_id.clone(),
            prerequisite: prereq_id.clone(),
            relation: None,
        })
        .unwrap();

        // Prereq goes to done via the cascade (normal path), which now also
        // creates a `ready` execution for dep. Simulate the pre-fix state
        // by manually demoting the execution back to `waiting_dependency`.
        db.update_work_item(
            &prereq_id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        // dep should now be `todo` with a `ready` execution (post-fix normal path).
        // Simulate the pre-fix stuck state: dep is `todo`, execution is `waiting_dependency`.
        db.force_execution_status_for_test(&dep_id, ExecutionStatus::WaitingDependency).unwrap();

        let executions_before = db.list_executions(Some(&dep_id)).unwrap();
        assert_eq!(executions_before.len(), 1);
        assert_eq!(executions_before[0].status, ExecutionStatus::WaitingDependency, "setup check");

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;

        assert_eq!(outcome.rows_stuck_promoted, 1, "sweep must promote the stuck execution");

        let executions_after = db.list_executions(Some(&dep_id)).unwrap();
        assert_eq!(executions_after.len(), 1);
        assert_eq!(executions_after[0].status, ExecutionStatus::Ready, "execution must be promoted to ready");
    }

    /// Part B: sweep must NOT promote a `todo` task that still has gating prereqs.
    #[tokio::test]
    async fn sweep_does_not_promote_still_gated_todo_task() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        // prereq has autostart=false so it won't appear in the promote scan
        // (which only scans autostart=1 tasks). This isolates the assertion to
        // dep only.
        let prereq_id = create_chore_no_autostart(&db, &product_id, "prereq");
        let dep_id = create_chore(&db, &product_id, "dependent");

        db.add_dependency(AddDependencyInput {
            dependent: dep_id.clone(),
            prerequisite: prereq_id.clone(),
            relation: None,
        })
        .unwrap();

        // dep is `blocked` (prereq not done). Directly force it to `todo` in
        // the DB (bypassing the gating check) to simulate a hypothetical stuck
        // state. The sweep must NOT promote the execution because gating
        // prereqs remain.
        db.force_task_status_for_test(&dep_id, "todo").unwrap();

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;

        assert_eq!(outcome.rows_stuck_promoted, 0, "must not promote while prereq is still todo");
    }
}
