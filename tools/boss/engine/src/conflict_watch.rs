//! Detection-trigger pipeline for merge-conflict handling on
//! `in_review` PRs (`tools/boss/docs/designs/merge-conflict-handling-in-review.md`).
//!
//! Two entry points, both invoked from `merge_poller::sweep_one`:
//!
//!   - [`on_conflict_detected`] — fired when the probe reports a PR
//!     in [`OpenPrMergeability::Conflict`]. Flips the parent
//!     `tasks` row from `in_review` to `blocked: merge_conflict`
//!     unless the auto-rebase flow already owns the slot (design
//!     Q7) or the WHERE-guard misses (human moved the row).
//!
//!   - [`on_resolved`] — fired when the probe reports a previously
//!     conflicting PR back in [`OpenPrMergeability::Clean`]. Flips
//!     the parent back to `in_review`, flips the engine-owned
//!     `conflict_resolutions` row to `succeeded`, and releases the
//!     worker's cube lease (design Q5). The WHERE guard ensures we
//!     only undo engine-owned transitions; a human who manually
//!     reclassified the row stays in charge.
//!
//! Both transitions are idempotent: a second call for the same
//! `(work_item, pr_url)` finds the row already in the target state
//! and updates zero rows, so re-firing on every sweep is harmless.
//!
//! Worker spawn lives in Phase 3 (`runner.rs`); this module reads the
//! attempt row written by that path to drive the retire side.
//!
//! [`OpenPrMergeability`]: crate::merge_poller::OpenPrMergeability

use boss_protocol::FrontendEvent;

use crate::coordinator::{CubeClient, ExecutionPublisher};
use crate::merge_poller::{PrLifecycleProbe, pr_labels_opt_out};
use crate::work::{PendingMergeCheck, WorkDb};

/// Decide whether the unified `auto_pr_maintenance_enabled` opt-out
/// (per-product flag or per-PR label) suppresses this conflict-watch
/// transition. Returns `true` to gate the path off, logging at debug
/// for traceability. DB-read errors fall through to "not opted out"
/// so a transient lookup failure doesn't silently drop a real signal —
/// the per-PR label is the second line of defence in that case.
///
/// Phase 6 #18 / design Q7: both gates fire on either the conflict
/// flip or the retire path; "opted out" means leave the row alone.
fn auto_pr_maintenance_disabled(
    work_db: &WorkDb,
    candidate: &PendingMergeCheck,
    labels: &[String],
) -> bool {
    if pr_labels_opt_out(labels) {
        tracing::debug!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            "conflict_watch: PR labelled with opt-out; skipping",
        );
        return true;
    }
    match work_db.product_auto_pr_maintenance_enabled(&candidate.product_id) {
        Ok(true) => false,
        Ok(false) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                product_id = %candidate.product_id,
                pr_url = %candidate.pr_url,
                "conflict_watch: product opted out of auto_pr_maintenance; skipping",
            );
            true
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                product_id = %candidate.product_id,
                ?err,
                "conflict_watch: failed to read auto_pr_maintenance_enabled; treating as enabled",
            );
            false
        }
    }
}

/// Fire-once flip from `in_review` to `blocked: merge_conflict`.
/// Returns `true` if the row actually transitioned (so the poller's
/// per-sweep counter can record it). All paths that *don't*
/// transition — WHERE-guard miss, auto-rebase owns the slot, DB
/// error — return `false` and log at the appropriate level.
///
/// When a freshly-inserted `conflict_resolutions` row accompanies the
/// flip (Phase 3 wiring), this path also publishes a typed
/// [`FrontendEvent::ConflictResolutionStarted`] envelope so activity-feed
/// subscribers can render the start-of-attempt entry without polling.
pub async fn on_conflict_detected(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
) -> bool {
    // Phase 6 #18 / Q7: the unified opt-out gates the entire flow.
    // Check it first so opted-out products never even probe the
    // rebase-attempt table or touch the parent row.
    if auto_pr_maintenance_disabled(work_db, candidate, &probe.labels) {
        return false;
    }
    // Q7: when `auto-rebase-stacked-prs` is already chasing this PR,
    // step aside. Auto-rebase escalation owns the slot until it
    // hits a terminal status; the next conflict-watch sweep will
    // re-evaluate once that resolves.
    match work_db.has_active_rebase_attempt_for_pr(&candidate.pr_url) {
        Ok(true) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "conflict_watch: rebase attempt active; deferring conflict flip",
            );
            return false;
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "conflict_watch: failed to check rebase attempt; deferring",
            );
            return false;
        }
    }
    let updated = match work_db
        .mark_chore_blocked_merge_conflict(&candidate.work_item_id, &candidate.pr_url)
    {
        Ok(Some(task)) => task,
        Ok(None) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "conflict_watch: WHERE guard missed; row already blocked or manually moved",
            );
            return false;
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "conflict_watch: failed to flip row to blocked: merge_conflict",
            );
            return false;
        }
    };
    publisher
        .publish_work_item_changed(
            &candidate.product_id,
            &updated.id,
            "blocked_merge_conflict",
        )
        .await;
    // If a `conflict_resolutions` row already exists for this work item
    // (Phase 3's worker-spawn path created it pre-flip, or a concurrent
    // sweep won the insert), emit the typed activity-feed event so the
    // macOS app can paint the "engine is on it" entry. The lookup is
    // best-effort — a failure here doesn't roll back the parent flip.
    if let Ok(Some(attempt)) =
        work_db.active_conflict_resolution_for_work_item(&candidate.work_item_id)
    {
        publisher
            .publish_frontend_event_on_product(
                &candidate.product_id,
                FrontendEvent::ConflictResolutionStarted {
                    product_id: candidate.product_id.clone(),
                    work_item_id: candidate.work_item_id.clone(),
                    attempt_id: attempt.id,
                    pr_url: candidate.pr_url.clone(),
                },
            )
            .await;
    }
    tracing::info!(
        work_item_id = %updated.id,
        kind = %updated.kind,
        pr_url = %candidate.pr_url,
        base_ref_oid = ?probe.base_ref_oid,
        "conflict_watch: PR conflicts with base; work item flipped to blocked: merge_conflict",
    );
    true
}

/// Symmetric resolution path: flip a `blocked: merge_conflict` row
/// back to `in_review` when the probe says the PR is mergeable
/// again. Returns `true` on transition.
///
/// The function is invoked even on the `in_review` sweep slice (a
/// `Clean` probe for an already-`in_review` row is a no-op via the
/// WHERE guard), so wiring stays simple — every `Clean` result
/// passes through here.
///
/// When an engine-owned `conflict_resolutions` row covers the chore
/// (Phase 3+), this path also (a) flips the attempt row to
/// `succeeded`, (b) releases the worker's cube workspace lease via
/// `cube_client`, and (c) broadcasts a typed
/// [`FrontendEvent::ConflictResolutionSucceeded`] envelope. `cube_client`
/// is taken as `Option` so tests / pre-Phase-3 wiring can run without a
/// cube. When `None`, the lease release is a no-op (the worker's
/// `record_worker_pr_completion` already released the lease on the Stop
/// event).
pub async fn on_resolved(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    cube_client: Option<&dyn CubeClient>,
    candidate: &PendingMergeCheck,
    labels: &[String],
) -> bool {
    // Phase 6 #18 / Q7: opt-out is symmetric — an opted-out product's
    // retire path is also a no-op so the engine doesn't undo a manual
    // intervention on a row it has stopped auto-managing.
    if auto_pr_maintenance_disabled(work_db, candidate, labels) {
        return false;
    }
    // Look up the engine-owned attempt row first. If one exists, drive
    // the strict (attempt-id-guarded) retire path; otherwise fall back
    // to the relaxed pr_url-only WHERE clause so this module still
    // closes the loop when Phase 3 wiring hasn't shipped yet.
    let attempt = match work_db
        .active_conflict_resolution_for_work_item(&candidate.work_item_id)
    {
        Ok(found) => found,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "conflict_watch: failed to look up active conflict_resolutions row; falling back to relaxed retire",
            );
            None
        }
    };

    let task_result = if let Some(attempt) = attempt.as_ref() {
        work_db.clear_chore_blocked_merge_conflict_for_attempt(
            &candidate.work_item_id,
            &candidate.pr_url,
            &attempt.id,
        )
    } else {
        work_db.clear_chore_blocked_merge_conflict(&candidate.work_item_id, &candidate.pr_url)
    };

    let task_transitioned = match task_result {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "conflict_watch: failed to clear blocked: merge_conflict",
            );
            return false;
        }
    };

    // The attempt row's update is independent of the parent flip. The
    // design (Q5) requires both to happen even if one of them has
    // already been moved by a concurrent path (manual override, on-Stop
    // completion, etc.).
    let mut attempt_transitioned = false;
    if let Some(attempt) = attempt.as_ref() {
        match work_db.mark_conflict_resolution_succeeded(&attempt.id, None) {
            Ok(Some(succeeded)) => {
                attempt_transitioned = true;
                // Release the cube workspace lease the attempt owned.
                // Idempotent on the cube side — the lease may already
                // have been released by the worker's on-Stop completion
                // path, in which case cube returns a benign error that
                // we log at debug.
                if let (Some(client), Some(lease_id)) =
                    (cube_client, succeeded.cube_lease_id.as_deref())
                {
                    if let Err(err) = client.release_workspace(lease_id).await {
                        tracing::debug!(
                            attempt_id = %succeeded.id,
                            lease_id,
                            ?err,
                            "conflict_watch: lease release on retire failed (likely already released)",
                        );
                    }
                }
                publisher
                    .publish_frontend_event_on_product(
                        &candidate.product_id,
                        FrontendEvent::ConflictResolutionSucceeded {
                            product_id: candidate.product_id.clone(),
                            work_item_id: candidate.work_item_id.clone(),
                            attempt_id: succeeded.id.clone(),
                            pr_url: candidate.pr_url.clone(),
                        },
                    )
                    .await;
            }
            Ok(None) => {
                tracing::debug!(
                    attempt_id = %attempt.id,
                    work_item_id = %candidate.work_item_id,
                    "conflict_watch: attempt row already terminal; skipping succeeded UPDATE",
                );
            }
            Err(err) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    ?err,
                    "conflict_watch: failed to mark conflict_resolution succeeded",
                );
            }
        }
    }

    if !task_transitioned && !attempt_transitioned {
        return false;
    }
    if task_transitioned {
        publisher
            .publish_work_item_changed(
                &candidate.product_id,
                &candidate.work_item_id,
                "merge_conflict_resolved",
            )
            .await;
    }
    tracing::info!(
        work_item_id = %candidate.work_item_id,
        pr_url = %candidate.pr_url,
        attempt_id = ?attempt.as_ref().map(|a| a.id.as_str()),
        task_transitioned,
        attempt_transitioned,
        "conflict_watch: PR mergeable again; retire path ran",
    );
    true
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    use super::*;
    use crate::coordinator::ExecutionPublisher;
    use crate::merge_poller::{OpenPrMergeability, PrLifecycleProbe, PrLifecycleState};
    use crate::work::{
        CreateChoreInput, CreateProductInput, WorkDb, WorkItem, WorkItemPatch,
    };

    #[derive(Default)]
    struct RecordingPublisher {
        events: Mutex<Vec<(String, String, String)>>,
        typed_events: Mutex<Vec<(String, FrontendEvent)>>,
    }

    #[async_trait]
    impl ExecutionPublisher for RecordingPublisher {
        async fn publish(&self, _: &str, _: &str, _: &str, _: &str) {}
        async fn publish_work_item_changed(
            &self,
            product_id: &str,
            work_item_id: &str,
            reason: &str,
        ) {
            self.events.lock().await.push((
                product_id.to_owned(),
                work_item_id.to_owned(),
                reason.to_owned(),
            ));
        }
        async fn publish_frontend_event_on_product(
            &self,
            product_id: &str,
            event: FrontendEvent,
        ) {
            self.typed_events
                .lock()
                .await
                .push((product_id.to_owned(), event));
        }
    }

    fn make_in_review(db: &WorkDb, name: &str, pr_url: &str) -> (String, String) {
        let product = db
            .create_product(CreateProductInput {
                name: format!("Product-{name}"),
                description: None,
                repo_remote_url: Some("git@github.com:foo/bar.git".into()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: name.into(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
            })
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".into()),
                pr_url: Some(pr_url.into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        (product.id, chore.id)
    }

    fn chore_status(db: &WorkDb, id: &str) -> (String, Option<String>) {
        match db.get_work_item(id).unwrap() {
            WorkItem::Chore(t) => (t.status, t.blocked_reason),
            other => panic!("expected chore, got {other:?}"),
        }
    }

    fn candidate(product_id: &str, work_item_id: &str, pr_url: &str) -> PendingMergeCheck {
        PendingMergeCheck {
            work_item_id: work_item_id.to_owned(),
            product_id: product_id.to_owned(),
            pr_url: pr_url.to_owned(),
        }
    }

    fn probe(pr_url: &str, state: PrLifecycleState) -> PrLifecycleProbe {
        PrLifecycleProbe {
            url: pr_url.to_owned(),
            state,
            base_ref_oid: Some("abc123".into()),
            labels: Vec::new(),
        }
    }

    fn probe_with_labels(
        pr_url: &str,
        state: PrLifecycleState,
        labels: &[&str],
    ) -> PrLifecycleProbe {
        PrLifecycleProbe {
            url: pr_url.to_owned(),
            state,
            base_ref_oid: Some("abc123".into()),
            labels: labels.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[tokio::test]
    async fn detection_flips_in_review_to_blocked_merge_conflict() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/10";
        let (product, chore) = make_in_review(&db, "C-detect", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        let transitioned = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        assert!(transitioned, "first detection must flip the row");

        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, "blocked");
        assert_eq!(reason.as_deref(), Some("merge_conflict"));

        let events = pub_.events.lock().await.clone();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            (product.clone(), chore.clone(), "blocked_merge_conflict".into())
        );
    }

    #[tokio::test]
    async fn detection_is_idempotent_on_repeated_probes() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/11";
        let (product, chore) = make_in_review(&db, "C-idem", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        let first = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        let second = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        assert!(first);
        assert!(!second, "second probe must be a no-op");
        let events = pub_.events.lock().await.clone();
        assert_eq!(events.len(), 1, "no second event from idempotent probe");
    }

    #[tokio::test]
    async fn resolution_flips_blocked_back_to_in_review() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/12";
        let (product, chore) = make_in_review(&db, "C-resolve", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        let resolved = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[]).await;
        assert!(resolved);

        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, "in_review");
        assert!(reason.is_none());

        let events = pub_.events.lock().await.clone();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].2, "merge_conflict_resolved");
    }

    #[tokio::test]
    async fn resolution_is_idempotent_on_repeated_clean_probes() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/13";
        let (product, chore) = make_in_review(&db, "C-clean-noop", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // First call: row is in_review (not blocked), so resolution is
        // a no-op — the WHERE guard misses, no event published.
        let r1 = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[]).await;
        assert!(!r1);
        assert!(pub_.events.lock().await.is_empty());

        // Drive a full conflict-resolve cycle, then call resolution
        // twice — the second call must also be a no-op.
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        let r2 = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[]).await;
        let r3 = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[]).await;
        assert!(r2);
        assert!(!r3);
    }

    #[tokio::test]
    async fn cycle_flip_resolve_flip() {
        // Integration: conflict → resolve → conflict again — all
        // transitions valid, all events fired, terminal state correct.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/14";
        let (product, chore) = make_in_review(&db, "C-cycle", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        assert!(
            on_conflict_detected(
                &db,
                pub_.as_ref(),
                &candidate(&product, &chore, pr),
                &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
            )
            .await
        );
        assert!(on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[]).await);
        assert!(
            on_conflict_detected(
                &db,
                pub_.as_ref(),
                &candidate(&product, &chore, pr),
                &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
            )
            .await
        );

        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, "blocked");
        assert_eq!(reason.as_deref(), Some("merge_conflict"));

        let reasons: Vec<String> = pub_
            .events
            .lock()
            .await
            .iter()
            .map(|(_, _, r)| r.clone())
            .collect();
        assert_eq!(
            reasons,
            vec![
                "blocked_merge_conflict".to_owned(),
                "merge_conflict_resolved".to_owned(),
                "blocked_merge_conflict".to_owned(),
            ],
        );
    }

    #[tokio::test]
    async fn detection_skipped_when_human_moved_row_off_in_review() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/15";
        let (product, chore) = make_in_review(&db, "C-human", pr);
        // Human flipped the row to `active` after PR was opened.
        db.update_work_item(
            &chore,
            WorkItemPatch {
                status: Some("active".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let pub_ = Arc::new(RecordingPublisher::default());

        let transitioned = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        assert!(!transitioned, "WHERE guard protects manual moves");
        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, "active");
        assert!(reason.is_none());
        assert!(pub_.events.lock().await.is_empty());
    }

    #[tokio::test]
    async fn resolution_skipped_when_human_moved_row_off_blocked() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/16";
        let (product, chore) = make_in_review(&db, "C-human-2", pr);
        let pub_ = Arc::new(RecordingPublisher::default());
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        // Human dropped the row from `blocked` back to `active` (e.g.
        // pulled the chore out of review themselves while the engine
        // was waiting).
        db.update_work_item(
            &chore,
            WorkItemPatch {
                status: Some("active".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let before_count = pub_.events.lock().await.len();
        let r = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[]).await;
        assert!(!r);
        assert_eq!(pub_.events.lock().await.len(), before_count);
    }

    /// `CubeClient` that records every `release_workspace` call so the
    /// retire-path tests can assert the lease release fired without
    /// standing up a real cube process.
    #[derive(Default)]
    struct RecordingCubeClient {
        releases: Mutex<Vec<String>>,
        release_should_fail: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl crate::coordinator::CubeClient for RecordingCubeClient {
        async fn ensure_repo(
            &self,
            _origin: &str,
        ) -> anyhow::Result<crate::coordinator::CubeRepoHandle> {
            unreachable!("not used in conflict_watch tests")
        }
        async fn lease_workspace(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<crate::coordinator::CubeWorkspaceLease> {
            unreachable!("not used in conflict_watch tests")
        }
        async fn create_change(
            &self,
            _: &std::path::PathBuf,
            _: &str,
        ) -> anyhow::Result<crate::coordinator::CubeChangeHandle> {
            unreachable!("not used in conflict_watch tests")
        }
        async fn release_workspace(&self, lease_id: &str) -> anyhow::Result<()> {
            self.releases.lock().await.push(lease_id.to_owned());
            if self
                .release_should_fail
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                Err(anyhow::anyhow!("simulated lease release failure"))
            } else {
                Ok(())
            }
        }
        async fn workspace_status(
            &self,
            _: &std::path::Path,
        ) -> anyhow::Result<crate::coordinator::CubeWorkspaceStatus> {
            unreachable!()
        }
        async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> anyhow::Result<()> {
            Ok(())
        }
        async fn force_release_lease(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list_workspaces(
            &self,
        ) -> anyhow::Result<Vec<crate::coordinator::CubeWorkspaceStatus>> {
            Ok(Vec::new())
        }
    }

    /// Insert a `conflict_resolutions` row in `running` for the given
    /// work item and stamp the parent's `blocked_attempt_id`. Mirrors
    /// what Phase 3's worker-spawn path will do at runtime; lets the
    /// retire-path tests run without standing up the worker pipeline.
    fn install_running_attempt(
        db: &WorkDb,
        product_id: &str,
        work_item_id: &str,
        pr_url: &str,
        lease_id: &str,
    ) -> String {
        let attempt = db
            .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
                product_id: product_id.to_owned(),
                work_item_id: work_item_id.to_owned(),
                pr_url: pr_url.to_owned(),
                pr_number: 99,
                head_branch: "feature".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("base-sha".into()),
                head_sha_before: Some("head-sha".into()),
            })
            .unwrap()
            .expect("attempt insert returns Some when no row exists yet");
        db.mark_conflict_resolution_running(&attempt.id, lease_id, "ws-1", "worker-1")
            .unwrap()
            .expect("mark_running must flip the freshly-inserted row");
        attempt.id
    }

    #[tokio::test]
    async fn retire_with_attempt_flips_parent_releases_lease_and_emits_typed_event() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/20";
        let (product, chore) = make_in_review(&db, "C-retire", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // Flip to blocked and install a running attempt so the retire
        // path has both a parent row and an attempt row to update.
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        let attempt_id = install_running_attempt(&db, &product, &chore, pr, "lease-42");

        let cube = Arc::new(RecordingCubeClient::default());
        let resolved = on_resolved(
            &db,
            pub_.as_ref(),
            Some(cube.as_ref() as &dyn crate::coordinator::CubeClient),
            &candidate(&product, &chore, pr),
            &[],
        )
        .await;
        assert!(resolved, "retire path must transition both rows");

        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, "in_review");
        assert!(reason.is_none());
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => {
                assert!(
                    t.blocked_attempt_id.is_none(),
                    "blocked_attempt_id must be cleared on retire",
                );
            }
            other => panic!("expected chore, got {other:?}"),
        }
        let attempt_row = db
            .get_conflict_resolution(&attempt_id)
            .unwrap()
            .expect("attempt row must still exist post-retire");
        assert_eq!(attempt_row.status, "succeeded");
        assert!(attempt_row.finished_at.is_some());

        assert_eq!(
            cube.releases.lock().await.as_slice(),
            ["lease-42"],
            "retire path must release the attempt's cube lease",
        );

        let work_events = pub_.events.lock().await.clone();
        assert!(
            work_events.iter().any(|(_, _, r)| r == "merge_conflict_resolved"),
            "expected merge_conflict_resolved work-item event, got {work_events:?}",
        );

        let typed = pub_.typed_events.lock().await.clone();
        let succeeded_event = typed
            .iter()
            .find(|(pid, ev)| {
                pid == &product
                    && matches!(
                        ev,
                        FrontendEvent::ConflictResolutionSucceeded { attempt_id: id, .. } if id == &attempt_id
                    )
            });
        assert!(
            succeeded_event.is_some(),
            "expected ConflictResolutionSucceeded event with attempt_id={attempt_id}, got {typed:?}",
        );
    }

    #[tokio::test]
    async fn typed_events_arrive_in_started_then_succeeded_order() {
        // Phase 4 #12 acceptance: a full conflict-resolve cycle emits
        // exactly one ConflictResolutionStarted and one
        // ConflictResolutionSucceeded, in that order, with matching
        // attempt_id payloads.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/25";
        let (product, chore) = make_in_review(&db, "C-evt-order", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // Phase 3 wiring: insert the attempt row before flipping the
        // parent so the started-event has a row to broadcast against.
        // (We invoke mark_chore_blocked_merge_conflict directly to set
        // up the insert pre-condition, then restore status to in_review
        // so on_conflict_detected has work to do.)
        db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
        let attempt = db
            .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
                product_id: product.clone(),
                work_item_id: chore.clone(),
                pr_url: pr.into(),
                pr_number: 25,
                head_branch: "feature".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("base-sha".into()),
                head_sha_before: Some("head-sha".into()),
            })
            .unwrap()
            .unwrap();
        db.mark_conflict_resolution_running(&attempt.id, "lease-25", "ws-25", "worker-25")
            .unwrap();
        db.update_work_item(
            &chore,
            WorkItemPatch {
                status: Some("in_review".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        let cube = Arc::new(RecordingCubeClient::default());
        on_resolved(
            &db,
            pub_.as_ref(),
            Some(cube.as_ref() as &dyn crate::coordinator::CubeClient),
            &candidate(&product, &chore, pr),
            &[],
        )
        .await;

        let typed = pub_.typed_events.lock().await.clone();
        let kinds: Vec<&'static str> = typed
            .iter()
            .map(|(_, ev)| match ev {
                FrontendEvent::ConflictResolutionStarted { .. } => "started",
                FrontendEvent::ConflictResolutionSucceeded { .. } => "succeeded",
                FrontendEvent::ConflictResolutionFailed { .. } => "failed",
                FrontendEvent::ConflictResolutionAbandoned { .. } => "abandoned",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["started", "succeeded"],
            "expected started → succeeded ordering, got {kinds:?}",
        );
        for (_, ev) in &typed {
            match ev {
                FrontendEvent::ConflictResolutionStarted { attempt_id: a, .. }
                | FrontendEvent::ConflictResolutionSucceeded { attempt_id: a, .. } => {
                    assert_eq!(a, &attempt.id, "attempt_id payload must match");
                }
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn retire_is_idempotent_on_repeated_probes_with_active_attempt() {
        // Second sweep over a row already retired must NOT re-emit
        // events nor re-release the cube lease.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/21";
        let (product, chore) = make_in_review(&db, "C-retire-idem", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        install_running_attempt(&db, &product, &chore, pr, "lease-99");

        let cube = Arc::new(RecordingCubeClient::default());
        let first = on_resolved(
            &db,
            pub_.as_ref(),
            Some(cube.as_ref() as &dyn crate::coordinator::CubeClient),
            &candidate(&product, &chore, pr),
            &[],
        )
        .await;
        let second = on_resolved(
            &db,
            pub_.as_ref(),
            Some(cube.as_ref() as &dyn crate::coordinator::CubeClient),
            &candidate(&product, &chore, pr),
            &[],
        )
        .await;
        assert!(first, "first retire transitions");
        assert!(!second, "second probe must be a no-op");

        assert_eq!(
            cube.releases.lock().await.len(),
            1,
            "lease must be released exactly once across duplicate probes",
        );
        let succeeded_count = pub_
            .typed_events
            .lock()
            .await
            .iter()
            .filter(|(_, ev)| matches!(ev, FrontendEvent::ConflictResolutionSucceeded { .. }))
            .count();
        assert_eq!(
            succeeded_count, 1,
            "ConflictResolutionSucceeded must fire exactly once across duplicate probes",
        );
    }

    #[tokio::test]
    async fn retire_tolerates_lease_release_failure() {
        // Cube release failures during retire must not block the
        // database transitions — the attempt is succeeded, the parent
        // is in_review, and we log + move on.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/22";
        let (product, chore) = make_in_review(&db, "C-retire-leasefail", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        let attempt_id = install_running_attempt(&db, &product, &chore, pr, "lease-zz");

        let cube = Arc::new(RecordingCubeClient::default());
        cube.release_should_fail
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let resolved = on_resolved(
            &db,
            pub_.as_ref(),
            Some(cube.as_ref() as &dyn crate::coordinator::CubeClient),
            &candidate(&product, &chore, pr),
            &[],
        )
        .await;
        assert!(resolved, "retire transitions must still report success");
        let attempt_row = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
        assert_eq!(attempt_row.status, "succeeded");
        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, "in_review");
        assert!(reason.is_none());
    }

    #[tokio::test]
    async fn detection_emits_started_event_when_attempt_row_exists() {
        // Phase 3 will create the conflict_resolutions row pre-flip;
        // the started-event publish is gated on that row existing.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/23";
        let (product, chore) = make_in_review(&db, "C-detect-evt", pr);

        // Move to blocked manually first so the attempt INSERT path's
        // task-side stamp matches its WHERE guard.
        db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
        let attempt = db
            .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
                product_id: product.clone(),
                work_item_id: chore.clone(),
                pr_url: pr.into(),
                pr_number: 23,
                head_branch: "feature".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("base-sha".into()),
                head_sha_before: Some("head-sha".into()),
            })
            .unwrap()
            .unwrap();
        // Reset to in_review so on_conflict_detected has work to do.
        db.update_work_item(
            &chore,
            WorkItemPatch {
                status: Some("in_review".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        let pub_ = Arc::new(RecordingPublisher::default());
        let transitioned = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        assert!(transitioned);

        let typed = pub_.typed_events.lock().await.clone();
        assert!(
            typed.iter().any(|(_, ev)| matches!(
                ev,
                FrontendEvent::ConflictResolutionStarted { attempt_id: a, .. } if a == &attempt.id
            )),
            "expected ConflictResolutionStarted event for attempt {}, got {typed:?}",
            attempt.id,
        );
    }

    #[tokio::test]
    async fn detection_emits_no_started_event_without_attempt_row() {
        // Pre-Phase-3: the parent flips to blocked but no attempt row
        // exists yet — the typed-event publish must be silent so we
        // don't broadcast an attempt-id-less Started.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/24";
        let (product, chore) = make_in_review(&db, "C-detect-noevt", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;

        let typed = pub_.typed_events.lock().await.clone();
        assert!(
            typed.is_empty(),
            "no Started event must fire when no attempt row exists yet, got {typed:?}",
        );
    }

    #[tokio::test]
    async fn detection_defers_when_rebase_attempt_is_active() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/17";
        let (product, chore) = make_in_review(&db, "C-rebase", pr);
        // Simulate auto-rebase having created its side table and a
        // running attempt for this PR. The table doesn't ship until
        // auto-rebase lands, so the conflict_watch must defer when it
        // does exist + has a non-terminal row.
        let conn = rusqlite::Connection::open(dir.path().join("boss.db")).unwrap();
        conn.execute(
            "CREATE TABLE rebase_attempts (
                 id                TEXT PRIMARY KEY,
                 dependent_pr_url  TEXT NOT NULL,
                 status            TEXT NOT NULL
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rebase_attempts (id, dependent_pr_url, status)
              VALUES ('reb_1', ?1, 'running')",
            [pr],
        )
        .unwrap();
        drop(conn);

        let pub_ = Arc::new(RecordingPublisher::default());
        let r = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        assert!(!r, "rebase-active path must defer");
        let (status, _) = chore_status(&db, &chore);
        assert_eq!(status, "in_review", "row stays where it was");
        assert!(pub_.events.lock().await.is_empty());
    }

    /// Flip `products.auto_pr_maintenance_enabled` directly on the
    /// SQLite file so opt-out tests can drive the gate without
    /// exposing a setter that production code doesn't yet need.
    fn set_product_auto_pr_maintenance(db_path: &std::path::Path, product_id: &str, enabled: bool) {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "UPDATE products SET auto_pr_maintenance_enabled = ?2 WHERE id = ?1",
            rusqlite::params![product_id, if enabled { 1 } else { 0 }],
        )
        .unwrap();
    }

    // ----- Phase 6 #18: opt-out gates conflict-watch flows -----

    #[tokio::test]
    async fn detection_skipped_when_product_opt_out_flag_disabled() {
        // Acceptance: an opted-out product's conflict-watch is a no-op.
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/600";
        let (product, chore) = make_in_review(&db, "C-optout-prod", pr);
        set_product_auto_pr_maintenance(&db_path, &product, false);

        let pub_ = Arc::new(RecordingPublisher::default());
        let r = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        assert!(!r, "opted-out product must not flip to blocked");
        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, "in_review");
        assert!(reason.is_none());
        assert!(pub_.events.lock().await.is_empty());
    }

    #[tokio::test]
    async fn detection_skipped_when_pr_has_opt_out_label() {
        // Per-PR label is the finer-grained opt-out — even on a
        // product with auto-maintenance enabled, a single labelled PR
        // is left alone.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/601";
        let (product, chore) = make_in_review(&db, "C-optout-label", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        let r = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe_with_labels(
                pr,
                PrLifecycleState::Open(OpenPrMergeability::Conflict),
                &["boss/no-auto-rebase"],
            ),
        )
        .await;
        assert!(!r, "labelled PR must not flip to blocked");
        let (status, _) = chore_status(&db, &chore);
        assert_eq!(status, "in_review");
        assert!(pub_.events.lock().await.is_empty());
    }

    #[tokio::test]
    async fn opt_out_label_match_is_case_insensitive() {
        // GitHub labels preserve case but the engine tolerates
        // BOSS/No-Auto-Rebase / etc. on the same gate so users don't
        // need to remember exact casing.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/602";
        let (product, chore) = make_in_review(&db, "C-optout-case", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        let r = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe_with_labels(
                pr,
                PrLifecycleState::Open(OpenPrMergeability::Conflict),
                &["Boss/No-Auto-Rebase"],
            ),
        )
        .await;
        assert!(!r);
    }

    #[tokio::test]
    async fn resolution_skipped_when_product_opt_out_flag_disabled() {
        // Symmetric retire-path gate: an opted-out product's retire
        // is also a no-op so the engine doesn't undo a manual
        // intervention on a row it has stopped auto-managing.
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/603";
        let (product, chore) = make_in_review(&db, "C-optout-retire", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // Flip into blocked with maintenance enabled so the row is
        // legitimately in `blocked: merge_conflict`; then flip the
        // product flag off and assert the retire path no-ops.
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrMergeability::Conflict)),
        )
        .await;
        let before = pub_.events.lock().await.len();
        set_product_auto_pr_maintenance(&db_path, &product, false);

        let r = on_resolved(
            &db,
            pub_.as_ref(),
            None,
            &candidate(&product, &chore, pr),
            &[],
        )
        .await;
        assert!(!r, "opted-out product must not retire automatically");
        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, "blocked");
        assert_eq!(reason.as_deref(), Some("merge_conflict"));
        assert_eq!(pub_.events.lock().await.len(), before);
    }

    // ----- Phase 6 #16: churn guard -----

    /// Re-open the SQLite file and back-date a `conflict_resolutions`
    /// row's `created_at` so churn-guard tests can simulate "this
    /// attempt is 30 minutes old without sleeping the test for 30
    /// minutes." Pure plumbing — production code never touches
    /// `created_at` after insert.
    fn rewind_attempt_created_at(db_path: &std::path::Path, attempt_id: &str, secs_ago: i64) {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let new_ts = (now_secs - secs_ago).to_string();
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "UPDATE conflict_resolutions SET created_at = ?2 WHERE id = ?1",
            rusqlite::params![attempt_id, new_ts],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn churn_guard_pre_abandons_fourth_attempt_in_window() {
        // Phase 6 #16 acceptance: 4 conflict-resolve cycles in <1h →
        // 4th attempt is abandoned with `churn_threshold_exceeded`.
        // We exercise the WorkDb insert path directly so the test
        // doesn't need to thread through a full worker-spawn cycle.
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/700";
        let (product, chore) = make_in_review(&db, "C-churn", pr);
        // Move parent into blocked so the insert path's task-side
        // stamp matches its WHERE guard for the live attempts.
        db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();

        // First three attempts inside the window go live.
        let make_input = |sha: &str| crate::work::ConflictResolutionInsertInput {
            product_id: product.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.into(),
            pr_number: 700,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some(sha.into()),
            head_sha_before: Some("head".into()),
        };
        let a1 = db.insert_conflict_resolution(make_input("sha-1")).unwrap().unwrap();
        let a2 = db.insert_conflict_resolution(make_input("sha-2")).unwrap().unwrap();
        let a3 = db.insert_conflict_resolution(make_input("sha-3")).unwrap().unwrap();
        for id in [&a1.id, &a2.id, &a3.id] {
            let row = db.get_conflict_resolution(id).unwrap().unwrap();
            assert_eq!(row.status, "pending", "first three attempts must be live");
            assert!(row.failure_reason.is_none());
        }

        // Fourth attempt — same hour — trips the guard.
        let a4 = db.insert_conflict_resolution(make_input("sha-4")).unwrap().unwrap();
        assert_eq!(
            a4.status, "abandoned",
            "fourth attempt inside the window must be pre-abandoned",
        );
        assert_eq!(
            a4.failure_reason.as_deref(),
            Some("churn_threshold_exceeded"),
            "failure_reason must record the guard",
        );
        assert!(
            a4.finished_at.is_some(),
            "pre-abandoned attempt must carry finished_at so it's terminal",
        );

        // Parent's `blocked_attempt_id` must still point at the
        // most-recent live attempt (a3), not the dead a4.
        match db.get_work_item(&chore).unwrap() {
            crate::work::WorkItem::Chore(t) => {
                assert_eq!(
                    t.blocked_attempt_id.as_deref(),
                    Some(a3.id.as_str()),
                    "blocked_attempt_id must not retarget at the pre-abandoned row",
                );
                assert_eq!(t.status, "blocked");
                assert_eq!(t.blocked_reason.as_deref(), Some("merge_conflict"));
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn churn_guard_does_not_count_attempts_older_than_window() {
        // The guard's window is rolling-1h. Back-date three attempts
        // to > 1h ago and a brand-new fourth must go live.
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/701";
        let (product, chore) = make_in_review(&db, "C-churn-rollover", pr);
        db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
        let make_input = |sha: &str| crate::work::ConflictResolutionInsertInput {
            product_id: product.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.into(),
            pr_number: 701,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some(sha.into()),
            head_sha_before: Some("head".into()),
        };
        let a1 = db.insert_conflict_resolution(make_input("sha-1")).unwrap().unwrap();
        let a2 = db.insert_conflict_resolution(make_input("sha-2")).unwrap().unwrap();
        let a3 = db.insert_conflict_resolution(make_input("sha-3")).unwrap().unwrap();
        // Push all three outside the 1h window (3700s > 3600s).
        for id in [&a1.id, &a2.id, &a3.id] {
            rewind_attempt_created_at(&db_path, id, 3_700);
        }

        let a4 = db.insert_conflict_resolution(make_input("sha-4")).unwrap().unwrap();
        assert_eq!(
            a4.status, "pending",
            "older-than-window attempts must not contribute to the guard",
        );
    }
}
