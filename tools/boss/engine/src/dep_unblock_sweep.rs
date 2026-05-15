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
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn so items blocked before engine boot are
/// recovered without waiting for the first interval.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    interval: Duration,
    metrics: Arc<Registry>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let outcome = run_one_pass(work_db.as_ref()).await;
            DEP_UNBLOCK_LONGEST_STALE_SECONDS.set(&metrics, outcome.longest_stale_secs as i64);
            tracing::info!(
                rows_evaluated = outcome.rows_evaluated,
                rows_unblocked = outcome.rows_unblocked,
                longest_stale_secs = outcome.longest_stale_secs,
                "dep-unblock sweep: pass complete",
            );
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

    outcome
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;
    use crate::work::{
        AddDependencyInput, CreateChoreInput, CreateProductInput, WorkDb, WorkItemPatch,
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
        assert_eq!(before.status, "blocked");
        assert_eq!(before.blocked_reason.as_deref(), Some("dependency"));

        // Mark prereq done without triggering the cascade.
        db.mark_task_done_for_test_no_cascade(&prereq_id).unwrap();

        // Dependent still blocked — cascade didn't fire.
        let dep_still = db.get_work_item(&dep_id).unwrap();
        let boss_protocol::WorkItem::Chore(still) = dep_still else { panic!() };
        assert_eq!(still.status, "blocked", "must still be blocked before sweep");

        // Sweep must recover it.
        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;

        assert_eq!(outcome.rows_evaluated, 1);
        assert_eq!(outcome.rows_unblocked, 1, "sweep must unblock the stale dependent");

        let dep_after = db.get_work_item(&dep_id).unwrap();
        let boss_protocol::WorkItem::Chore(after) = dep_after else { panic!() };
        assert_eq!(after.status, "todo");
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
        assert_eq!(after.status, "todo");
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
            after.status, "todo",
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
        assert_eq!(after.status, "blocked");
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
}
