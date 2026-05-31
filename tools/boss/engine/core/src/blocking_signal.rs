//! Shared blocking-signal → revision remediation driver.
//!
//! `conflict_watch` (the `merge_conflict` signal) and `ci_watch` (the
//! `ci_failure` signal) both turn a blocking signal on an `in_review` PR into
//! a spawned `kind=revision` fix task. Historically each path re-implemented
//! the parent-state machine, so a fix to one (notably #1007's
//! "keep the revision's parent in Review while the fix is in flight") had to
//! be — and repeatedly was *not* — mirrored to the other.
//!
//! The parent-state transitions they share are written **once** here,
//! parameterised by [`SignalKind`]:
//!
//! - keep the parent in `in_review` while a fix revision is in flight
//!   ([`unblock_for_revision`]);
//! - reconcile a parent that is already `blocked: <reason>` but has an active
//!   fix revision back to `in_review`
//!   ([`reconcile_blocked_parent_with_revision`]);
//! - re-arm the side-table signal so the polymorphic clear keeps firing
//!   ([`SignalKind::rearm_blocked_signal`]).
//!
//! Signal-specific bits stay in the per-signal watchers: the cleared probe
//! (`mergeable != CONFLICTING` vs required checks green), the attempt ledger
//! (`conflict_resolutions` vs `ci_remediations`), and the CI-only concerns
//! (attempt budget / exhaustion, manual-override suppression, never-starts
//! in-flight alerts, the `retrigger` / `merge_queue_rebounce` attempt kinds).
//! The shared driver only owns the parent's `tasks.status` /
//! `task_blocked_signals` transitions, which are identical across signals.

use crate::work::{PendingMergeCheck, WorkDb};

/// Which blocking signal a remediation flow is handling. The variants map to
/// the `tasks.blocked_reason` / `task_blocked_signals.reason` literals and
/// select the kind-specific DB transitions the shared driver dispatches to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalKind {
    /// The PR no longer merges cleanly against its base
    /// (`mergeable = CONFLICTING`). Attempt ledger: `conflict_resolutions`.
    MergeConflict,
    /// The PR's required checks are failing. Attempt ledger: `ci_remediations`.
    CiFailure,
}

impl SignalKind {
    /// The `blocked_reason` literal for the still-trying state.
    pub fn reason(self) -> &'static str {
        match self {
            SignalKind::MergeConflict => "merge_conflict",
            SignalKind::CiFailure => "ci_failure",
        }
    }

    /// Clear the parent's `blocked: <reason>` flip back to `in_review` (the
    /// relaxed, non-attempt-guarded variant). Returns `true` when a row was
    /// flipped.
    fn clear_blocked(
        self,
        db: &WorkDb,
        work_item_id: &str,
        pr_url: &str,
    ) -> anyhow::Result<bool> {
        let cleared = match self {
            SignalKind::MergeConflict => {
                db.clear_chore_blocked_merge_conflict(work_item_id, pr_url)?
            }
            SignalKind::CiFailure => db.clear_chore_blocked_ci_failure(work_item_id, pr_url)?,
        };
        Ok(cleared.is_some())
    }

    /// Upsert the in-flight side-table signal so `maybe_clear_blocked` keeps
    /// dispatching the kind's retire probe while the parent stays `in_review`.
    fn record_in_flight(
        self,
        db: &WorkDb,
        work_item_id: &str,
        attempt_id: &str,
    ) -> anyhow::Result<()> {
        match self {
            SignalKind::MergeConflict => {
                db.record_merge_conflict_in_flight(work_item_id, attempt_id)
            }
            SignalKind::CiFailure => db.record_ci_failure_in_flight(work_item_id, attempt_id),
        }
    }

    /// Re-arm the side-table signal for a parent already `blocked: <reason>`.
    /// Returns `true` iff the parent is in that blocked state (so the caller
    /// can distinguish a human-moved row from the re-arm scenario).
    pub fn rearm_blocked_signal(self, db: &WorkDb, work_item_id: &str) -> anyhow::Result<bool> {
        match self {
            SignalKind::MergeConflict => db.rearm_blocked_merge_conflict_signal(work_item_id),
            SignalKind::CiFailure => db.rearm_blocked_ci_failure_signal(work_item_id),
        }
    }
}

/// Shared post-spawn reconciliation: a revision fix vehicle is (or is already)
/// in flight, so undo the upfront `blocked: <reason>` flip and upsert the
/// in-flight signal. The parent stays in the Review column while the revision
/// runs in Doing — the #1007 model, written once for both signals.
///
/// Returns `true` when the parent was cleared back to `in_review` on this
/// call. Failures are logged and swallowed (mirrors the original conflict
/// path): a failed clear leaves the parent `blocked`, which the next sweep
/// re-reconciles via [`reconcile_blocked_parent_with_revision`].
pub fn unblock_for_revision(
    db: &WorkDb,
    kind: SignalKind,
    candidate: &PendingMergeCheck,
    attempt_id: &str,
) -> bool {
    let cleared = match kind.clear_blocked(db, &candidate.work_item_id, &candidate.pr_url) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                reason = kind.reason(),
                ?err,
                "blocking_signal: failed to clear blocked after revision spawn; parent may be stuck",
            );
            false
        }
    };
    if let Err(err) = kind.record_in_flight(db, &candidate.work_item_id, attempt_id) {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            reason = kind.reason(),
            attempt_id,
            ?err,
            "blocking_signal: failed to record in-flight signal; retire probe may not fire",
        );
    }
    cleared
}

/// Shared catch-up reconciliation for a parent that is `blocked: <reason>` but
/// already has an active fix revision in flight (e.g. a row that was blocked
/// before the in_review model shipped, or two detection events landing in the
/// same sweep). Clears the block back to `in_review` and re-records the
/// in-flight signal so the revision card in Doing is the user-visible signal.
///
/// Returns `true` when the parent was reconciled to `in_review`.
pub fn reconcile_blocked_parent_with_revision(
    db: &WorkDb,
    kind: SignalKind,
    candidate: &PendingMergeCheck,
    attempt_id: &str,
) -> bool {
    unblock_for_revision(db, kind, candidate, attempt_id)
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{CreateChoreInput, CreateProductInput, WorkItem, WorkItemPatch};
    use std::path::PathBuf;

    fn in_review_chore(db: &WorkDb, pr: &str) -> (String, String) {
        let product = db
            .create_product(CreateProductInput {
                name: "P".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:foo/bar.git".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "c".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                pr_url: Some(pr.to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        (product.id, chore.id)
    }

    fn status_of(db: &WorkDb, id: &str) -> (String, Option<String>) {
        match db.get_work_item(id).unwrap() {
            WorkItem::Chore(t) => (t.status, t.blocked_reason),
            other => panic!("expected chore, got {other:?}"),
        }
    }

    /// Flip the parent into `blocked: <reason>` for the given kind, mirroring
    /// the upfront flip the watcher does before spawning a revision.
    fn block_parent(db: &WorkDb, kind: SignalKind, chore: &str, pr: &str) {
        match kind {
            SignalKind::MergeConflict => {
                db.mark_chore_blocked_merge_conflict(chore, pr).unwrap();
            }
            SignalKind::CiFailure => {
                db.mark_chore_blocked_ci_failure(chore, pr, None).unwrap();
            }
        }
    }

    fn candidate(product: &str, chore: &str, pr: &str) -> PendingMergeCheck {
        PendingMergeCheck {
            work_item_id: chore.to_owned(),
            product_id: product.to_owned(),
            pr_url: pr.to_owned(),
        }
    }

    fn signal_active(db: &WorkDb, chore: &str, reason: &str) -> bool {
        db.active_blocked_signals(chore)
            .unwrap()
            .iter()
            .any(|s| s.reason == reason)
    }

    // The shared driver runs identically for both signal kinds - these tests
    // are parameterised so a conflict-only or ci-only regression cannot hide.
    const KINDS: [SignalKind; 2] = [SignalKind::MergeConflict, SignalKind::CiFailure];

    #[test]
    fn unblock_for_revision_keeps_parent_in_review_for_both_kinds() {
        for kind in KINDS {
            let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
            let pr = "https://github.com/foo/bar/pull/1";
            let (product, chore) = in_review_chore(&db, pr);
            block_parent(&db, kind, &chore, pr);
            let (status, _) = status_of(&db, &chore);
            assert_eq!(status, "blocked", "{kind:?}: precondition blocked");

            let cleared =
                unblock_for_revision(&db, kind, &candidate(&product, &chore, pr), "att-1");
            assert!(cleared, "{kind:?}: parent must clear back to in_review");
            let (status, reason) = status_of(&db, &chore);
            assert_eq!(status, "in_review", "{kind:?}");
            assert!(reason.is_none(), "{kind:?}");
            assert!(
                signal_active(&db, &chore, kind.reason()),
                "{kind:?}: in-flight signal must stay armed",
            );
        }
    }

    #[test]
    fn reconcile_blocked_parent_with_revision_for_both_kinds() {
        for kind in KINDS {
            let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
            let pr = "https://github.com/foo/bar/pull/2";
            let (product, chore) = in_review_chore(&db, pr);
            block_parent(&db, kind, &chore, pr);

            let reconciled = reconcile_blocked_parent_with_revision(
                &db,
                kind,
                &candidate(&product, &chore, pr),
                "att-2",
            );
            assert!(
                reconciled,
                "{kind:?}: pre-blocked parent must reconcile to in_review",
            );
            let (status, _) = status_of(&db, &chore);
            assert_eq!(status, "in_review", "{kind:?}");
            assert!(signal_active(&db, &chore, kind.reason()), "{kind:?}");
        }
    }

    #[test]
    fn rearm_blocked_signal_distinguishes_blocked_from_in_review_for_both_kinds() {
        for kind in KINDS {
            let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
            let pr = "https://github.com/foo/bar/pull/3";
            let (_product, chore) = in_review_chore(&db, pr);

            // A parent left in_review is not re-armed (returns false).
            assert!(
                !kind.rearm_blocked_signal(&db, &chore).unwrap(),
                "{kind:?}: in_review parent must not re-arm",
            );

            // A blocked parent is re-armed (returns true) and the signal is set.
            block_parent(&db, kind, &chore, pr);
            assert!(
                kind.rearm_blocked_signal(&db, &chore).unwrap(),
                "{kind:?}: blocked parent must re-arm",
            );
            assert!(signal_active(&db, &chore, kind.reason()), "{kind:?}");
        }
    }
}
