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

use boss_protocol::{CREATED_VIA_MERGE_CONFLICT_PREFIX, CreateRevisionInput, FrontendEvent};

use crate::coordinator::{CubeClient, ExecutionPublisher};
use crate::merge_poller::{PrLifecycleProbe, parse_pr_number, pr_labels_opt_out};
use crate::work::{
    ConflictResolutionInsertInput, PendingMergeCheck, PrStateChecker, WorkDb,
};

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
///
/// Phase 3 cutover (design Q1/Q5): on a *genuinely-new* attempt row the
/// fix vehicle is now an **engine-triggered revision** (`parent = chore`,
/// `created_via = "merge-conflict:<crz_id>"`) created via the shared
/// `create_revision` gate, rather than a bespoke `conflict_resolution`
/// execution. The producer reuses the create-time `assert_parent_revisable`
/// gate (R4) and stamps `attempt.revision_task_id`; the reconcile loop then
/// dispatches a `revision_implementation` execution into the chain root's
/// warm workspace. `pr_checker` supplies the gate's live PR-state probe
/// (`&GhPrStateChecker` in production, a fake in tests).
pub async fn on_conflict_detected(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    pr_checker: &dyn PrStateChecker,
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
    // Try to flip the parent from `in_review` → `blocked: merge_conflict`.
    // This is the primary path (new conflict). If the WHERE guard misses, we
    // check the stale-crz re-arm path before giving up (T230 scenario: the
    // task is already blocked but the previous resolution worker ran against
    // an obsolete base SHA, so GitHub still reports CONFLICTING).
    let task_flipped_now = match work_db
        .mark_chore_blocked_merge_conflict(&candidate.work_item_id, &candidate.pr_url)
    {
        Ok(Some(_task)) => true,
        Ok(None) => {
            // WHERE guard missed. Two sub-cases:
            // (a) Human moved the row — leave it alone.
            // (b) Task IS blocked:merge_conflict with no active crz —
            //     the previous resolution targeted a stale base. Re-arm
            //     the signal and let the crz-insert path below dispatch
            //     a fresh attempt against the current base SHA.
            let is_blocked = match work_db
                .rearm_blocked_merge_conflict_signal(&candidate.work_item_id)
            {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!(
                        work_item_id = %candidate.work_item_id,
                        ?err,
                        "conflict_watch: failed to check/rearm blocked signal; skipping",
                    );
                    return false;
                }
            };
            if !is_blocked {
                tracing::debug!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    "conflict_watch: WHERE guard missed; row not blocked:merge_conflict (manually moved); skipping",
                );
                return false;
            }
            // Task IS blocked:merge_conflict; signal re-armed.
            //
            // Check for an active (pending/running) crz. If one exists, the
            // worker is still in flight — no new dispatch needed. If none
            // exists, check the most recent crz's terminal status:
            //   - `succeeded`: the worker resolved against a stale base SHA
            //     (T230 scenario). Re-arm — fall through to insert a fresh crz
            //     against the current base SHA.
            //   - `failed`/`abandoned`: the churn guard or human already owns
            //     the retry decision; do NOT automatically re-dispatch.
            match work_db.active_conflict_resolution_for_work_item(&candidate.work_item_id) {
                Ok(Some(_)) => {
                    tracing::debug!(
                        work_item_id = %candidate.work_item_id,
                        pr_url = %candidate.pr_url,
                        "conflict_watch: blocked signal re-armed; active crz still in flight; no new dispatch",
                    );
                    return false;
                }
                Ok(None) => {
                    // No active crz. Check the most recent crz status to
                    // decide whether to re-arm.
                    let latest_status = match work_db
                        .latest_conflict_resolution_for_work_item(&candidate.work_item_id)
                    {
                        Ok(Some(crz)) => crz.status,
                        Ok(None) => {
                            // No crz at all — this is a fresh block, not
                            // a stale-base scenario. The insert path will
                            // handle it.
                            "pending".to_owned()
                        }
                        Err(err) => {
                            tracing::warn!(
                                work_item_id = %candidate.work_item_id,
                                ?err,
                                "conflict_watch: failed to read latest crz during re-arm; skipping dispatch",
                            );
                            return false;
                        }
                    };
                    match latest_status.as_str() {
                        "succeeded" => {
                            // Previous worker succeeded against an obsolete base.
                            // Fall through to dispatch against the current base SHA.
                            tracing::info!(
                                work_item_id = %candidate.work_item_id,
                                pr_url = %candidate.pr_url,
                                base_ref_oid = ?probe.base_ref_oid,
                                "conflict_watch: stale-base re-arm: succeeded crz but PR still CONFLICTING; dispatching fresh attempt",
                            );
                        }
                        "pending" => {
                            // No previous crz (or brand-new pending one) — fall
                            // through to the insert path; it handles idempotency
                            // via the UNIQUE key guard.
                        }
                        other => {
                            // failed / abandoned — churn guard or human owns retry.
                            tracing::debug!(
                                work_item_id = %candidate.work_item_id,
                                terminal_status = other,
                                "conflict_watch: blocked signal re-armed; latest crz terminal ({other}); churn guard owns retry",
                            );
                            return false;
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        work_item_id = %candidate.work_item_id,
                        ?err,
                        "conflict_watch: failed to check active crz during re-arm; skipping dispatch",
                    );
                    return false;
                }
            }
            // task didn't flip (it was already blocked), but we proceed
            // with the crz-insert path below.
            false
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

    // Only publish the "newly blocked" work-item event when the task actually
    // flipped status (not in the re-arm path where it was already blocked).
    if task_flipped_now {
        publisher
            .publish_work_item_changed(
                &candidate.product_id,
                &candidate.work_item_id,
                "blocked_merge_conflict",
            )
            .await;
    }

    // Insert the `conflict_resolutions` attempt row now that the parent
    // is blocked. The UNIQUE key is `(work_item_id, base_sha_at_trigger)`,
    // so a second sweep for the same base sha returns `Ok(None)` —
    // idempotent and safe to call on every conflict-detected event.
    // In the re-arm path the base SHA is the *current* main SHA (different
    // from the stale crz's base_sha_at_trigger), so a new row is inserted.
    // The churn guard pre-abandons the 4th attempt inside a rolling 1h
    // window; those rows get no execution request.
    let attempt = match work_db.insert_conflict_resolution(ConflictResolutionInsertInput {
        product_id: candidate.product_id.clone(),
        work_item_id: candidate.work_item_id.clone(),
        pr_url: candidate.pr_url.clone(),
        pr_number: parse_pr_number(&candidate.pr_url).unwrap_or(0),
        head_branch: probe.head_ref_name.as_deref().unwrap_or("").to_owned(),
        base_branch: probe.base_ref_name.as_deref().unwrap_or("").to_owned(),
        base_sha_at_trigger: probe.base_ref_oid.clone(),
        head_sha_before: probe.head_ref_oid.clone(),
    }) {
        Ok(Some(a)) => Some(a),
        Ok(None) => {
            // UNIQUE collision — a row for this base sha already exists.
            // Fall back to a lookup so the started-event still fires.
            work_db
                .active_conflict_resolution_for_work_item(&candidate.work_item_id)
                .unwrap_or(None)
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "conflict_watch: failed to insert conflict_resolution attempt; continuing without execution request",
            );
            None
        }
    };

    // Phase 3 cutover (design Q1/Q5): on a genuinely-new live attempt,
    // create an engine-triggered revision as the fix vehicle instead of a
    // bespoke `conflict_resolution` execution. The `revision_task_id`
    // soft-FK is the idempotency latch: a fresh `insert_conflict_resolution`
    // returns a row with it NULL; a same-base-sha repeat probe hits the
    // existing row (already stamped) and skips. Pre-abandoned churn-guard
    // rows (`status != 'pending'`) get no revision, so the parent stays
    // `blocked: merge_conflict` for human attention — the cap is enforced
    // here, before create, exactly as the old execution-request guard was.
    if let Some(ref a) = attempt {
        if a.status == "pending" && a.revision_task_id.is_none() {
            maybe_spawn_conflict_revision(work_db, publisher, pr_checker, candidate, probe, a).await;
        }
    }

    if let Some(ref a) = attempt {
        publisher
            .publish_frontend_event_on_product(
                &candidate.product_id,
                FrontendEvent::ConflictResolutionStarted {
                    product_id: candidate.product_id.clone(),
                    work_item_id: candidate.work_item_id.clone(),
                    attempt_id: a.id.clone(),
                    pr_url: candidate.pr_url.clone(),
                },
            )
            .await;
    }

    tracing::info!(
        work_item_id = %candidate.work_item_id,
        pr_url = %candidate.pr_url,
        base_ref_oid = ?probe.base_ref_oid,
        attempt_id = ?attempt.as_ref().map(|a| a.id.as_str()),
        attempt_status = ?attempt.as_ref().map(|a| a.status.as_str()),
        task_flipped_now,
        "conflict_watch: PR conflicts with base; conflict detection ran",
    );
    task_flipped_now
}

/// Create the engine-triggered revision that delivers the conflict fix and
/// stamp the trigger-ledger row's `revision_task_id` back-pointer (design
/// Q1/Q2/Q5).
///
/// `attempt` is the just-inserted, live (`pending`) `conflict_resolutions`
/// row. On success the reconcile loop picks up the new `kind=revision` task
/// and dispatches a `revision_implementation` execution into the chain
/// root's warm workspace. On failure — almost always the create-time gate
/// (`assert_parent_revisable`, R4) refusing a parent whose PR has since
/// merged/closed, occasionally a transient `gh` probe error — the ledger
/// row is marked `abandoned` so it never strands as a `pending` attempt
/// with no fix vehicle (which the dormant backfill/rescue paths would
/// otherwise try to dispatch). The parent stays `blocked: merge_conflict`;
/// the poller's merged/closed handling reconciles it on a later sweep.
async fn maybe_spawn_conflict_revision(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    pr_checker: &dyn PrStateChecker,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
    attempt: &crate::work::ConflictResolution,
) {
    let base_branch = probe
        .base_ref_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("main");
    // Short, one-line card title (design Q3 / R5): generated from the base
    // branch, never the diagnosis body. The long worker directive
    // (diagnosis tables, step-by-step rebase recipe) is injected at
    // dispatch by `compose_revision_directive`, keyed off `created_via`
    // (Phase 2).
    let description = format!("Resolve merge conflict against {base_branch}");
    let created_via = format!("{CREATED_VIA_MERGE_CONFLICT_PREFIX}{}", attempt.id);

    let revision = match work_db.create_revision(
        CreateRevisionInput {
            parent_task_id: candidate.work_item_id.clone(),
            description,
            name: None,
            priority: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
            created_via: Some(created_via),
        },
        pr_checker,
    ) {
        Ok(rev) => rev,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                attempt_id = %attempt.id,
                error = %format!("{err:#}"),
                "conflict_watch: create_revision failed (parent likely no longer revisable); abandoning attempt",
            );
            if let Err(abandon_err) =
                work_db.mark_conflict_resolution_abandoned(&attempt.id, "revision_create_failed")
            {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    ?abandon_err,
                    "conflict_watch: failed to abandon attempt after create_revision failure",
                );
            }
            return;
        }
    };

    // Stamp the reverse link. This is the idempotency latch (repeat probes
    // at the same base sha find it set and skip) and the marker that tells
    // the dormant backfill/rescue paths to leave this row alone.
    match work_db.set_conflict_resolution_revision_task_id(&attempt.id, &revision.id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            tracing::warn!(
                attempt_id = %attempt.id,
                revision_task_id = %revision.id,
                "conflict_watch: attempt row vanished before revision_task_id could be stamped",
            );
        }
        Err(err) => {
            tracing::warn!(
                attempt_id = %attempt.id,
                revision_task_id = %revision.id,
                ?err,
                "conflict_watch: failed to stamp revision_task_id; revision will still run",
            );
        }
    }

    tracing::info!(
        work_item_id = %candidate.work_item_id,
        pr_url = %candidate.pr_url,
        attempt_id = %attempt.id,
        revision_task_id = %revision.id,
        "conflict_watch: spawned engine-triggered revision for merge conflict",
    );

    // Nudge the scheduler so the reconcile loop dispatches the revision's
    // `revision_implementation` execution promptly rather than waiting for
    // the next opportunistic kick.
    publisher.kick_scheduler();
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
    //
    // For a `pending` attempt (PR resolved before the worker started) we
    // only call mark_conflict_resolution_succeeded when the parent task was
    // also cleared in this call.  If task_transitioned is false the attempt
    // is either stale (idempotent second probe) or the task was moved by a
    // human — in both cases we leave the attempt alone.
    let mut attempt_transitioned = false;
    if let Some(attempt) = attempt.as_ref() {
        let should_succeed = attempt.status == "running" || task_transitioned;
        if should_succeed {
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
    use crate::merge_poller::{OpenPrStatus, PrLifecycleProbe, PrLifecycleState};
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
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
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
                force_duplicate: false,
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
            head_ref_oid: Some("head456".into()),
            head_ref_name: Some("feature".into()),
            base_ref_name: Some("main".into()),
            labels: Vec::new(),
            review: crate::merge_poller::PrReviewState::Unknown,
            in_merge_queue: false,
        }
    }

    /// A `PrStateChecker` that reports every PR as `Open`, so the
    /// `create_revision` create-time gate passes for the in-review chore
    /// fixtures these tests build. A conflicting PR is, by definition,
    /// still open — matching what `GhPrStateChecker` returns in production.
    fn open_checker() -> crate::work::FakePrStateChecker {
        crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open)
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
            head_ref_oid: Some("head456".into()),
            head_ref_name: Some("feature".into()),
            base_ref_name: Some("main".into()),
            labels: labels.iter().map(|s| (*s).to_owned()).collect(),
            review: crate::merge_poller::PrReviewState::Unknown,
            in_merge_queue: false,
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
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
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
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;
        let second = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
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
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
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
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
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
                &open_checker(),
                &candidate(&product, &chore, pr),
                &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
            )
            .await
        );
        assert!(on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[]).await);
        assert!(
            on_conflict_detected(
                &db,
                pub_.as_ref(),
                &open_checker(),
                &candidate(&product, &chore, pr),
                &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
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
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
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
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
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
        async fn list_repos(
            &self,
        ) -> anyhow::Result<Vec<crate::coordinator::CubeRepoSummary>> {
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
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
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
        // Full conflict-resolve cycle: on_conflict_detected emits
        // ConflictResolutionStarted; on_resolved emits Succeeded; both
        // events carry the same attempt_id.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/25";
        let (product, chore) = make_in_review(&db, "C-evt-order", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // on_conflict_detected creates the attempt and emits Started.
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;

        let started_attempt_id = {
            let typed = pub_.typed_events.lock().await.clone();
            match typed
                .iter()
                .find(|(_, ev)| matches!(ev, FrontendEvent::ConflictResolutionStarted { .. }))
            {
                Some((_, FrontendEvent::ConflictResolutionStarted { attempt_id, .. })) => {
                    attempt_id.clone()
                }
                other => panic!("expected ConflictResolutionStarted, got {other:?}"),
            }
        };

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
                    assert_eq!(a, &started_attempt_id, "attempt_id payload must match");
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
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
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
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
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
    async fn detection_emits_started_event_reuses_existing_row_on_same_base_sha() {
        // When on_conflict_detected is called a second time for the same
        // base sha (UNIQUE key collision), insert_conflict_resolution
        // returns Ok(None) and we fall back to the existing attempt for
        // the started-event. Both events must reference the original row.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/23";
        let (product, chore) = make_in_review(&db, "C-detect-evt", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // First call — creates the attempt and flips to blocked.
        let first = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;
        assert!(first);
        let first_events = pub_.typed_events.lock().await.clone();
        assert_eq!(first_events.len(), 1, "exactly one started event on first call");
        let first_attempt_id = match &first_events[0].1 {
            FrontendEvent::ConflictResolutionStarted { attempt_id, .. } => attempt_id.clone(),
            other => panic!("unexpected event {other:?}"),
        };

        // Reset to in_review with the same probe (same base sha → UNIQUE collision).
        db.update_work_item(
            &chore,
            WorkItemPatch {
                status: Some("in_review".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        let second = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;
        assert!(second, "second call with same pr must still flip the row");

        let all_events = pub_.typed_events.lock().await.clone();
        let started: Vec<_> = all_events
            .iter()
            .filter(|(_, ev)| matches!(ev, FrontendEvent::ConflictResolutionStarted { .. }))
            .collect();
        assert_eq!(started.len(), 2, "started event fires on each flip");
        for (_, ev) in &started {
            match ev {
                FrontendEvent::ConflictResolutionStarted { attempt_id: a, .. } => {
                    assert_eq!(
                        a, &first_attempt_id,
                        "both events must reference the same original attempt",
                    );
                }
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn detection_inserts_attempt_and_emits_started_event() {
        // on_conflict_detected now inserts the conflict_resolution attempt
        // and emits ConflictResolutionStarted in the same call that flips
        // the parent to blocked — no pre-wiring needed.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/24";
        let (product, chore) = make_in_review(&db, "C-detect-noevt", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        let transitioned = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;
        assert!(transitioned);

        let attempt = db
            .active_conflict_resolution_for_work_item(&chore)
            .unwrap();
        assert!(
            attempt.is_some(),
            "on_conflict_detected must insert a conflict_resolution row",
        );
        let attempt = attempt.unwrap();
        assert_eq!(attempt.status, "pending");

        let typed = pub_.typed_events.lock().await.clone();
        assert!(
            typed.iter().any(|(_, ev)| matches!(
                ev,
                FrontendEvent::ConflictResolutionStarted { attempt_id: a, .. } if a == &attempt.id
            )),
            "ConflictResolutionStarted must fire with the new attempt id, got {typed:?}",
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
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
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
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
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
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe_with_labels(
                pr,
                PrLifecycleState::Open(OpenPrStatus::conflict_only()),
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
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe_with_labels(
                pr,
                PrLifecycleState::Open(OpenPrStatus::conflict_only()),
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
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
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

    // ----- Phase 3 cutover: engine-triggered revision as the fix vehicle -----

    #[tokio::test]
    async fn detection_spawns_revision_and_stamps_attempt() {
        // A genuinely-new conflict creates a `kind=revision` task (parent =
        // chore, merge-conflict provenance), stamps the ledger row's
        // `revision_task_id`, and creates NO bespoke conflict_resolution
        // execution — the dormant path stays dormant and the row is hidden
        // from the backfill/rescue recovery queries.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/30";
        let (product, chore) = make_in_review(&db, "C-rev-spawn", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        assert!(
            on_conflict_detected(
                &db,
                pub_.as_ref(),
                &open_checker(),
                &candidate(&product, &chore, pr),
                &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
            )
            .await
        );

        let attempt = db
            .active_conflict_resolution_for_work_item(&chore)
            .unwrap()
            .expect("a pending attempt row must exist");
        assert_eq!(attempt.status, "pending");
        let rev_id = attempt
            .revision_task_id
            .clone()
            .expect("the producer must stamp revision_task_id on the attempt");

        let revision = match db.get_work_item(&rev_id).unwrap() {
            WorkItem::Task(t) => t,
            other => panic!("expected revision task, got {other:?}"),
        };
        assert_eq!(revision.kind, "revision");
        assert_eq!(revision.parent_task_id.as_deref(), Some(chore.as_str()));
        assert_eq!(revision.created_via, format!("merge-conflict:{}", attempt.id));
        assert_eq!(revision.description, "Resolve merge conflict against main");

        // No bespoke conflict_resolution execution: the revision rides the
        // reconcile loop's revision_implementation dispatch instead.
        let ready = db.list_ready_executions().unwrap();
        assert!(
            !ready.iter().any(|e| e.kind == "conflict_resolution"),
            "cutover must not create a conflict_resolution execution; got {ready:?}",
        );

    }

    #[tokio::test]
    async fn detection_idempotent_does_not_double_spawn_revision() {
        // Re-firing on the same base sha reuses the existing attempt (whose
        // revision_task_id is already set) and spawns no second revision.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/31";
        let (product, chore) = make_in_review(&db, "C-rev-idem", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;
        // Reset to in_review so the second probe re-enters the primary flip
        // path with the same base sha (UNIQUE collision on the ledger).
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
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;

        let attempts = db
            .list_conflict_resolutions(None, &[], Some(&chore), None)
            .unwrap();
        assert_eq!(attempts.len(), 1, "same base sha must not stack attempts");
        let revision_backed = attempts
            .iter()
            .filter(|r| r.revision_task_id.is_some())
            .count();
        assert_eq!(revision_backed, 1, "exactly one revision-backed attempt");
    }

    #[tokio::test]
    async fn churn_abandoned_attempt_spawns_no_revision() {
        // The 4th conflict in the rolling window is pre-abandoned by the
        // churn guard; the producer's `status == 'pending'` guard means it
        // gets no revision (the cap is enforced before create).
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/32";
        let (product, chore) = make_in_review(&db, "C-rev-churn", pr);

        // Three prior attempts in the window arm the guard. Plant them while
        // the chore is still `in_review` so the producer's primary flip path
        // (not the re-arm short-circuit) reaches the insert for the fourth.
        for sha in ["s1", "s2", "s3"] {
            db.insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
                product_id: product.clone(),
                work_item_id: chore.clone(),
                pr_url: pr.into(),
                pr_number: 32,
                head_branch: "feature".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some(sha.into()),
                head_sha_before: Some("head".into()),
            })
            .unwrap();
        }

        let pub_ = Arc::new(RecordingPublisher::default());
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &open_checker(),
            &candidate(&product, &chore, pr),
            // probe base is "abc123" — a fourth distinct sha in the window.
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;

        let fourth = db
            .list_conflict_resolutions(None, &[], Some(&chore), None)
            .unwrap()
            .into_iter()
            .find(|r| r.base_sha_at_trigger.as_deref() == Some("abc123"))
            .expect("fourth attempt row must exist");
        assert_eq!(fourth.status, "abandoned");
        assert_eq!(
            fourth.failure_reason.as_deref(),
            Some("churn_threshold_exceeded"),
        );
        assert!(
            fourth.revision_task_id.is_none(),
            "churn-abandoned attempt must spawn no revision",
        );
    }

    #[tokio::test]
    async fn create_revision_failure_abandons_attempt() {
        // When the create-time gate refuses (parent PR no longer open, R4),
        // the producer marks the ledger row `abandoned` so it never strands
        // as a pending attempt with no fix vehicle, and spawns no revision.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/33";
        let (product, chore) = make_in_review(&db, "C-rev-fail", pr);
        let pub_ = Arc::new(RecordingPublisher::default());
        let closed =
            crate::work::FakePrStateChecker::always(crate::work::PrOpenState::ClosedUnmerged);

        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &closed,
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;

        // The parent flip precedes the gate, so the chore is still blocked;
        // the poller's merged/closed handling reconciles it on a later sweep.
        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, "blocked");
        assert_eq!(reason.as_deref(), Some("merge_conflict"));

        let attempts = db
            .list_conflict_resolutions(None, &[], Some(&chore), None)
            .unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].status, "abandoned");
        assert_eq!(
            attempts[0].failure_reason.as_deref(),
            Some("revision_create_failed"),
        );
        assert!(attempts[0].revision_task_id.is_none());
    }
}
