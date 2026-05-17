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
use crate::merge_poller::{PrLifecycleProbe, parse_pr_number, pr_labels_opt_out};
use crate::work::{
    ConflictResolutionInsertInput, CreateExecutionInput, PendingMergeCheck,
    StrandedConflictAttempt, WorkDb,
};

/// One-shot startup backfill: create a `ready` `conflict_resolution`
/// execution for every `conflict_resolutions` row that is `pending` but
/// has no `work_executions` entry. This recovers attempts that were
/// inserted by `on_conflict_detected` before PR #430 wired the
/// `create_execution` call into the same handler (rows written at ~17:07
/// UTC 2026-05-13 for task `task_18af2d5bc18e2b48_25`, attempt
/// `crz_18af37b7f0da0898_1`).
///
/// The DB query is idempotent (NOT EXISTS predicate), so a second engine
/// restart after the backfill finds zero rows and logs nothing.
pub fn backfill_orphaned_executions(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
) {
    let orphans = match work_db.pending_conflict_resolutions_without_execution() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(
                ?err,
                "conflict_watch backfill: failed to query orphaned conflict_resolutions; skipping",
            );
            return;
        }
    };
    if orphans.is_empty() {
        tracing::debug!("conflict_watch backfill: no orphaned conflict_resolutions found");
        return;
    }
    let mut backfilled = 0usize;
    for attempt in &orphans {
        tracing::debug!(
            attempt_id = %attempt.id,
            work_item_id = %attempt.work_item_id,
            "conflict_watch backfill: creating execution_request for orphaned attempt",
        );
        match work_db.create_execution(CreateExecutionInput {
            work_item_id: attempt.work_item_id.clone(),
            kind: "conflict_resolution".to_owned(),
            status: Some("ready".to_owned()),
            repo_remote_url: None,
            cube_repo_id: None,
            cube_lease_id: None,
            cube_workspace_id: None,
            workspace_path: None,
            priority: None,
            preferred_workspace_id: None,
            started_at: None,
            finished_at: None,
        }) {
            Ok(_) => {
                backfilled += 1;
                publisher.kick_scheduler();
            }
            Err(err) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    work_item_id = %attempt.work_item_id,
                    ?err,
                    "conflict_watch backfill: failed to create execution; attempt will remain undispatched",
                );
            }
        }
    }
    tracing::info!(
        backfilled,
        "conflict_watch backfill: created execution_request for orphaned conflict_resolutions",
    );
}

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

    // Request a conflict_resolution execution for live (pending) attempts.
    // Pre-abandoned churn-guard rows get no execution so the scheduler
    // doesn't dispatch a worker that would immediately fail the same way.
    if let Some(ref a) = attempt {
        if a.status == "pending" {
            match work_db.create_execution(CreateExecutionInput {
                work_item_id: candidate.work_item_id.clone(),
                kind: "conflict_resolution".to_owned(),
                status: Some("ready".to_owned()),
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            }) {
                Ok(_) => publisher.kick_scheduler(),
                Err(err) => {
                    tracing::warn!(
                        work_item_id = %candidate.work_item_id,
                        attempt_id = %a.id,
                        ?err,
                        "conflict_watch: failed to create conflict_resolution execution; worker will not be dispatched",
                    );
                }
            }
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

/// Re-emit a `conflict_resolution` execution for a stranded pending
/// attempt — one where the `conflict_resolutions` row is `pending` but
/// no live execution exists. This happens when the engine restarts after
/// the attempt row was inserted but before `create_execution` ran, or
/// when the dispatched worker dies before reaching a terminal state
/// (leaving the attempt stuck in `pending` with no live execution).
///
/// Only the product-level opt-out is checked here; the PR-label opt-out
/// requires a fresh GH probe that this recovery path intentionally skips
/// to avoid extra network round-trips.  Label changes are rare and the
/// normal probe sweep will catch them on the next pass.
///
/// Returns `true` if a new execution was successfully emitted.
pub async fn rescue_stranded_attempt(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    attempt: &StrandedConflictAttempt,
) -> bool {
    let candidate = PendingMergeCheck {
        work_item_id: attempt.work_item_id.clone(),
        product_id: attempt.product_id.clone(),
        pr_url: attempt.pr_url.clone(),
    };
    if auto_pr_maintenance_disabled(work_db, &candidate, &[]) {
        return false;
    }
    match work_db.create_execution(CreateExecutionInput {
        work_item_id: attempt.work_item_id.clone(),
        kind: "conflict_resolution".to_owned(),
        status: Some("ready".to_owned()),
        repo_remote_url: None,
        cube_repo_id: None,
        cube_lease_id: None,
        cube_workspace_id: None,
        workspace_path: None,
        priority: None,
        preferred_workspace_id: None,
        started_at: None,
        finished_at: None,
    }) {
        Ok(_) => {
            publisher.kick_scheduler();
            tracing::info!(
                work_item_id = %attempt.work_item_id,
                attempt_id = %attempt.attempt_id,
                pr_url = %attempt.pr_url,
                "conflict_watch: re-dispatched execution for stranded pending attempt",
            );
            true
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %attempt.work_item_id,
                attempt_id = %attempt.attempt_id,
                ?err,
                "conflict_watch: failed to re-emit execution for stranded attempt",
            );
            false
        }
    }
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
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;
        let second = on_conflict_detected(
            &db,
            pub_.as_ref(),
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

    // ----- backfill_orphaned_executions -----

    /// Publisher that counts `kick_scheduler` calls so the backfill
    /// tests can assert the scheduler was nudged for each recovered row.
    #[derive(Default)]
    struct KickCountingPublisher {
        kicks: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl ExecutionPublisher for KickCountingPublisher {
        async fn publish(&self, _: &str, _: &str, _: &str, _: &str) {}
        async fn publish_work_item_changed(&self, _: &str, _: &str, _: &str) {}
        async fn publish_frontend_event_on_product(&self, _: &str, _: FrontendEvent) {}
        fn kick_scheduler(&self) {
            self.kicks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Directly insert a `pending` conflict_resolution row into `db`
    /// without also creating an execution — this simulates the
    /// pre-PR-#430 state where on_conflict_detected wrote the attempt
    /// but not the execution.
    fn plant_pending_attempt(
        db: &WorkDb,
        product_id: &str,
        work_item_id: &str,
        pr_url: &str,
        sha: &str,
    ) -> String {
        db.insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
            product_id: product_id.to_owned(),
            work_item_id: work_item_id.to_owned(),
            pr_url: pr_url.to_owned(),
            pr_number: 427,
            head_branch: "feat".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some(sha.into()),
            head_sha_before: Some("head".into()),
        })
        .unwrap()
        .expect("attempt insert must succeed for a fresh (work_item, sha) key")
        .id
    }

    #[test]
    fn backfill_creates_execution_for_pending_orphan() {
        // (a) A stranded pending attempt with no execution gets a ready
        // execution after backfill_orphaned_executions runs.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/427";
        let (product, chore) = make_in_review(&db, "B-pending", pr);
        db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();

        let attempt_id = plant_pending_attempt(&db, &product, &chore, pr, "sha-orphan");
        let pub_ = KickCountingPublisher::default();

        backfill_orphaned_executions(&db, &pub_);

        // Exactly one execution must now exist for this work item.
        let executions = db
            .pending_conflict_resolutions_without_execution()
            .unwrap();
        assert!(
            executions.is_empty(),
            "after backfill the NOT-EXISTS query must return no rows; got {executions:?}",
        );
        assert_eq!(
            pub_.kicks.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "scheduler must be kicked once per recovered attempt",
        );
        // Verify the attempt row itself is still pending — the backfill
        // only adds the execution, not the worker side-effects.
        let row = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
        assert_eq!(row.status, "pending");
    }

    #[test]
    fn backfill_skips_abandoned_attempts() {
        // (b) An abandoned (churn-guard) attempt must NOT receive an
        // execution — the guard explicitly marked it dead.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/428";
        let (product, chore) = make_in_review(&db, "B-abandoned", pr);
        db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();

        // Insert three attempts first so the fourth trips the churn guard.
        for sha in ["s1", "s2", "s3"] {
            plant_pending_attempt(&db, &product, &chore, pr, sha);
        }
        // Fourth insert — churn guard fires, returns abandoned.
        let dead = db
            .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
                product_id: product.clone(),
                work_item_id: chore.clone(),
                pr_url: pr.into(),
                pr_number: 428,
                head_branch: "feat".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("s4".into()),
                head_sha_before: Some("head".into()),
            })
            .unwrap()
            .unwrap();
        assert_eq!(dead.status, "abandoned");

        let pub_ = KickCountingPublisher::default();
        backfill_orphaned_executions(&db, &pub_);

        // The abandoned row must not appear in the orphan query.
        let orphans = db
            .pending_conflict_resolutions_without_execution()
            .unwrap();
        // The three live (pending) attempts don't have executions yet
        // so they will be in the list; abandoned must not be.
        for o in &orphans {
            assert_ne!(
                o.id, dead.id,
                "abandoned attempt must never appear in orphan backfill query",
            );
        }
    }

    #[test]
    fn backfill_is_idempotent_when_execution_already_exists() {
        // (c) An attempt that already has an execution must not get a
        // duplicate — the NOT EXISTS guard excludes it on the second run.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/429";
        let (product, chore) = make_in_review(&db, "B-idem", pr);
        db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();

        plant_pending_attempt(&db, &product, &chore, pr, "sha-idem");

        let pub_ = KickCountingPublisher::default();
        // First run — should create one execution.
        backfill_orphaned_executions(&db, &pub_);
        let kicks_after_first =
            pub_.kicks.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(kicks_after_first, 1);

        // Second run — no new executions should be created.
        backfill_orphaned_executions(&db, &pub_);
        let kicks_after_second =
            pub_.kicks.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            kicks_after_second, 1,
            "second backfill must be a no-op; kick count must not increase",
        );
    }
}
