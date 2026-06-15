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
#[cfg(test)]
use boss_protocol::{ExecutionKind, TaskKind};

use crate::blocking_signal::{self, SignalKind};
use crate::coordinator::{CubeClient, ExecutionPublisher};
use crate::merge_poller::{PrLifecycleProbe, parse_pr_number, pr_labels_opt_out};
#[cfg(test)]
use crate::work::TaskStatus;
use crate::work::{ConflictResolutionInsertInput, PendingMergeCheck, PrStateChecker, WorkDb};

/// Decide whether the unified `auto_pr_maintenance_enabled` opt-out
/// (per-product flag or per-PR label) suppresses this conflict-watch
/// transition. Returns `true` to gate the path off, logging at debug
/// for traceability. DB-read errors fall through to "not opted out"
/// so a transient lookup failure doesn't silently drop a real signal —
/// the per-PR label is the second line of defence in that case.
///
/// Phase 6 #18 / design Q7: both gates fire on either the conflict
/// flip or the retire path; "opted out" means leave the row alone.
fn auto_pr_maintenance_disabled(work_db: &WorkDb, candidate: &PendingMergeCheck, labels: &[String]) -> bool {
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

/// Conflict-detection entry point — creates a `conflict_resolutions`
/// attempt and dispatches an engine-triggered revision fix vehicle when
/// the probe reports `OpenPrMergeability::Conflict`.
///
/// **Parent-state model (post-revision-unification):**
/// While an active conflict-resolution revision is in flight, the
/// parent stays in `in_review` (Review column) — exactly as a normal
/// revision leaves its parent. The parent flips to
/// `blocked: merge_conflict` only when there is no tractable fix
/// vehicle: the churn cap was exceeded, or `create_revision` failed
/// (parent PR no longer revisable). That is the genuine "needs a
/// human" terminal.
///
/// Implementation note: we still call `mark_chore_blocked_merge_conflict`
/// as the upfront WHERE guard (it enforces `status='in_review'` to
/// protect against human-moved rows), then immediately clear it back to
/// `in_review` and upsert the `task_blocked_signals` row whenever a
/// revision is successfully spawned. The brief intermediate `blocked`
/// state is invisible to the sweep — the entire detect → spawn → unblock
/// sequence runs within a single call.
///
/// Returns `true` when the parent task status changed (in either
/// direction) or a fresh attempt row was created; `false` for purely
/// idempotent repeat probes and human-owned rows.
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
    // Pre-flight: when an active revision fix vehicle already exists for this
    // work item, the detection flow is essentially a no-op for an `in_review`
    // parent (signal already armed, revision already in Doing). Skip the
    // upfront flip+unblock cycle to avoid redundant state changes on every
    // sweep.  The blocked-parent reconciliation (T791/T898) is handled below
    // via the re-arm path; we fall through there when `rearm` says blocked.
    match work_db.active_conflict_resolution_for_work_item(&candidate.work_item_id) {
        Ok(Some(ref active_crz)) if active_crz.revision_task_id.is_some() => {
            match work_db.rearm_blocked_merge_conflict_signal(&candidate.work_item_id) {
                Ok(true) => {
                    // Parent is blocked with an active revision in flight — fall
                    // through to the reconciliation path in the re-arm branch.
                }
                Ok(false) | Err(_) => {
                    // Parent is in_review (or human-moved): idempotent probe.
                    // Re-arm the signal so maybe_clear_blocked continues to fire
                    // when the PR becomes clean, then return false (no net change).
                    let _ = work_db.record_merge_conflict_in_flight(&candidate.work_item_id, &active_crz.id);
                    tracing::debug!(
                        work_item_id = %candidate.work_item_id,
                        attempt_id = %active_crz.id,
                        "conflict_watch: active revision in flight; idempotent probe no-op",
                    );
                    return false;
                }
            }
        }
        _ => {}
    }

    // Try to flip the parent from `in_review` → `blocked: merge_conflict`.
    // The WHERE guard (`status = 'in_review'`) is load-bearing: it protects
    // rows a human moved away from `in_review` (return false, leave alone).
    // If the guard misses because the task is already `blocked: merge_conflict`,
    // we fall into the stale-crz re-arm path below.
    //
    // IMPORTANT (post-revision-unification): if `maybe_spawn_conflict_revision`
    // succeeds, we immediately clear this flip back to `in_review` and upsert
    // the signal row — the parent stays in Review while the fix is in flight.
    // The flip is only kept when there is NO active fix vehicle (churn cap,
    // create_revision failure).
    let task_flipped_to_blocked = match work_db
        .mark_chore_blocked_merge_conflict(&candidate.work_item_id, &candidate.pr_url)
    {
        Ok(Some(_task)) => true,
        Ok(None) => {
            // WHERE guard missed. Two sub-cases:
            // (a) Human moved the row — leave it alone.
            // (b) Task IS blocked:merge_conflict — check for an active revision
            //     fix vehicle and reconcile if found (post-revision-unification
            //     catch-up for rows that were blocked before this model shipped),
            //     or dispatch a fresh attempt for the stale-base scenario.
            let is_blocked = match work_db.rearm_blocked_merge_conflict_signal(&candidate.work_item_id) {
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
            // Check for an active (pending/running) crz.
            //   - Active crz with revision_task_id: the fix vehicle is in
            //     flight but the parent is erroneously blocked (pre-model-
            //     change rows like T791/T898). Reconcile by clearing the block
            //     so the parent returns to Review.
            //   - Active crz without revision_task_id: old-style bespoke
            //     execution still running — leave blocked, no new dispatch.
            //   - No active crz: check latest terminal status for stale-base
            //     re-arm vs churn-guard terminal.
            match work_db.active_conflict_resolution_for_work_item(&candidate.work_item_id) {
                Ok(Some(active_crz)) => {
                    if active_crz.revision_task_id.is_some() {
                        // Active revision fix vehicle, but parent is blocked.
                        // This is the reconciliation path for rows that were
                        // blocked before the revision-unification model shipped.
                        // Flip parent back to in_review; the revision card in
                        // Doing is the user-visible "something is happening."
                        tracing::info!(
                            work_item_id = %candidate.work_item_id,
                            pr_url = %candidate.pr_url,
                            attempt_id = %active_crz.id,
                            revision_task_id = %active_crz.revision_task_id.as_deref().unwrap_or(""),
                            "conflict_watch: active revision in flight but parent blocked; reconciling to in_review",
                        );
                        let reconciled = match work_db
                            .clear_chore_blocked_merge_conflict(&candidate.work_item_id, &candidate.pr_url)
                        {
                            Ok(Some(_)) => true,
                            Ok(None) => false,
                            Err(err) => {
                                tracing::warn!(
                                    work_item_id = %candidate.work_item_id,
                                    ?err,
                                    "conflict_watch: failed to reconcile block during re-arm",
                                );
                                false
                            }
                        };
                        if reconciled {
                            if let Err(err) =
                                work_db.record_merge_conflict_in_flight(&candidate.work_item_id, &active_crz.id)
                            {
                                tracing::warn!(
                                    work_item_id = %candidate.work_item_id,
                                    ?err,
                                    "conflict_watch: failed to record in-flight signal during reconcile",
                                );
                            }
                            publisher
                                .publish_work_item_changed(
                                    &candidate.product_id,
                                    &candidate.work_item_id,
                                    "conflict_revision_in_flight",
                                )
                                .await;
                        }
                        publisher
                            .publish_frontend_event_on_product(
                                &candidate.product_id,
                                FrontendEvent::ConflictResolutionStarted {
                                    product_id: candidate.product_id.clone(),
                                    work_item_id: candidate.work_item_id.clone(),
                                    attempt_id: active_crz.id.clone(),
                                    pr_url: candidate.pr_url.clone(),
                                },
                            )
                            .await;
                        tracing::info!(
                            work_item_id = %candidate.work_item_id,
                            reconciled,
                            "conflict_watch: re-arm reconciliation complete",
                        );
                        return reconciled;
                    }
                    // Old-style crz (no revision), still in flight.
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
                    let latest_status = match work_db.latest_conflict_resolution_for_work_item(&candidate.work_item_id)
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
            // task was already blocked (re-arm path), didn't flip here.
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

    // Insert the `conflict_resolutions` attempt row. The UNIQUE key is
    // `(work_item_id, base_sha_at_trigger)`, so a second sweep for the
    // same base sha returns `Ok(None)` — idempotent and safe to call on
    // every conflict-detected event. In the re-arm path the base SHA is
    // the *current* main SHA (different from the stale crz's
    // base_sha_at_trigger), so a new row is inserted. The churn guard
    // pre-abandons the 4th attempt inside a rolling 1h window.
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

    // Phase 3 cutover / post-revision-unification parent-state model:
    //
    // For a genuinely-new live attempt, create an engine-triggered revision.
    // If the revision spawns successfully (or an existing revision is already
    // in flight via a UNIQUE-collision path):
    //   - Clear the task back to `in_review` (undoing the upfront flip).
    //   - Upsert the `task_blocked_signals` row so `maybe_clear_blocked`
    //     dispatches `on_resolved` when the PR later becomes mergeable.
    //   - Parent stays in Review column while the fix is in Doing.
    // If the revision fails (create_revision gate refused) or the churn cap
    // pre-abandoned the attempt:
    //   - Keep the `blocked: merge_conflict` flip (no revision vehicle means
    //     the parent must surface in the Blocked column for human attention).
    // The "clear the upfront flip back to in_review + record the in-flight
    // signal" sequence is the #1007 parent-state model, now written once in
    // [`crate::blocking_signal`] and shared with the CI-failure path.
    let mut task_unblocked_for_revision = false;

    if let Some(ref a) = attempt {
        if a.status == "pending" && a.revision_task_id.is_none() {
            // Fresh attempt — try to spawn a revision.
            let spawned = maybe_spawn_conflict_revision(work_db, publisher, pr_checker, candidate, probe, a).await;
            if spawned {
                task_unblocked_for_revision =
                    blocking_signal::unblock_for_revision(work_db, SignalKind::MergeConflict, candidate, &a.id);
            }
            // If !spawned: attempt abandoned (revision_create_failed). Parent
            // stays `blocked: merge_conflict`.
        } else if a.revision_task_id.is_some() && task_flipped_to_blocked {
            // UNIQUE collision: existing revision in flight (repeat probe at
            // same base sha). The upfront flip to blocked was premature — clear
            // it back so the parent stays in Review while the fix continues.
            task_unblocked_for_revision =
                blocking_signal::unblock_for_revision(work_db, SignalKind::MergeConflict, candidate, &a.id);
        }
        // a.status == "abandoned" (churn guard) with no revision_task_id:
        // parent stays blocked — this is the human-attention terminal.
    }

    // Publish parent state-change event.
    // - Flipped to blocked (churn cap, create_revision failure, UNIQUE-collision
    //   with no active revision): "blocked_merge_conflict"
    // - Fix vehicle spawned (parent is now/stays `in_review` with revision
    //   in Doing): "conflict_revision_in_flight"
    // - Pure no-op (idempotent UNIQUE collision with existing revision): no event
    if task_unblocked_for_revision {
        publisher
            .publish_work_item_changed(
                &candidate.product_id,
                &candidate.work_item_id,
                "conflict_revision_in_flight",
            )
            .await;
    } else if task_flipped_to_blocked {
        publisher
            .publish_work_item_changed(&candidate.product_id, &candidate.work_item_id, "blocked_merge_conflict")
            .await;
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
        task_flipped_to_blocked,
        task_unblocked_for_revision,
        raw_mergeable = %probe.raw_mergeable,
        raw_merge_state_status = %probe.raw_merge_state_status,
        "conflict_watch: PR conflicts with base; conflict detection ran",
    );
    task_flipped_to_blocked || task_unblocked_for_revision
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
///
/// Returns `true` when the revision was successfully created and
/// `revision_task_id` was stamped; `false` on any failure (attempt
/// abandoned). The caller uses this to decide whether to flip the parent
/// back to `in_review` or leave it `blocked: merge_conflict`.
async fn maybe_spawn_conflict_revision(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    pr_checker: &dyn PrStateChecker,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
    attempt: &crate::work::ConflictResolution,
) -> bool {
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
        CreateRevisionInput::builder()
            .parent_task_id(candidate.work_item_id.clone())
            .description(description)
            .created_via(created_via)
            .build(),
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
            if let Err(abandon_err) = work_db.mark_conflict_resolution_abandoned(&attempt.id, "revision_create_failed")
            {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    ?abandon_err,
                    "conflict_watch: failed to abandon attempt after create_revision failure",
                );
            }
            return false;
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
    true
}

/// Symmetric resolution path: retire the active `conflict_resolutions`
/// attempt when the probe says the PR is mergeable again. Returns `true`
/// on any transition (task or attempt row updated).
///
/// **Post-revision-unification:** the parent task may be in either
/// `blocked: merge_conflict` (no-fix-vehicle terminal) OR `in_review`
/// (revision was in flight). Both cases are handled:
///
/// - `blocked: merge_conflict` → flip to `in_review`, retire attempt,
///   publish `merge_conflict_resolved` work-item event (classic path).
/// - `in_review` (parent never left Review) → skip the task flip, clear
///   the `merge_conflict` signal from `task_blocked_signals`, retire
///   attempt, publish `ConflictResolutionSucceeded` typed event. No
///   `merge_conflict_resolved` work-item event (the parent didn't
///   change status).
///
/// The WHERE guard on the strict clear path still protects rows a human
/// moved elsewhere — only engine-owned `blocked: merge_conflict` rows
/// or rows with an active `merge_conflict` signal are touched.
pub async fn on_resolved(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    cube_client: Option<&dyn CubeClient>,
    candidate: &PendingMergeCheck,
    labels: &[String],
    raw_mergeable: &str,
    raw_merge_state_status: &str,
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
    let attempt = match work_db.active_conflict_resolution_for_work_item(&candidate.work_item_id) {
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
        work_db.clear_chore_blocked_merge_conflict_for_attempt(&candidate.work_item_id, &candidate.pr_url, &attempt.id)
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
    // For a `pending` attempt we mark succeeded when:
    //   (a) The parent was blocked and we just cleared it (`task_transitioned`).
    //   (b) The parent was `in_review` (revision fix vehicle — the WHERE guard
    //       on the task clear missed, but the attempt itself should retire).
    //       We detect this via `revision_task_id` being set: a pending attempt
    //       with a revision always corresponds to the new-model in-flight path.
    //   (c) The attempt was already `running` (worker was active).
    let mut attempt_transitioned = false;
    if let Some(attempt) = attempt.as_ref() {
        let parent_in_review_with_revision =
            !task_transitioned && attempt.status == "pending" && attempt.revision_task_id.is_some();
        let should_succeed = attempt.status == "running" || task_transitioned || parent_in_review_with_revision;
        if should_succeed {
            match work_db.mark_conflict_resolution_succeeded(&attempt.id, None) {
                Ok(Some(succeeded)) => {
                    attempt_transitioned = true;
                    // When the parent was `in_review` (never blocked), clear the
                    // `merge_conflict` signal so `maybe_clear_blocked` does not
                    // re-trigger on the next probe.
                    if parent_in_review_with_revision
                        && let Err(err) = work_db.clear_merge_conflict_signal_only(&candidate.work_item_id)
                    {
                        tracing::warn!(
                            work_item_id = %candidate.work_item_id,
                            ?err,
                            "conflict_watch: failed to clear in-flight signal after retire",
                        );
                    }
                    // Release the cube workspace lease the attempt owned.
                    // Idempotent on the cube side — the lease may already
                    // have been released by the worker's on-Stop completion
                    // path, in which case cube returns a benign error that
                    // we log at debug.
                    if let (Some(client), Some(lease_id)) = (cube_client, succeeded.cube_lease_id.as_deref())
                        && let Err(err) = client.release_workspace(lease_id).await
                    {
                        tracing::debug!(
                            attempt_id = %succeeded.id,
                            lease_id,
                            ?err,
                            "conflict_watch: lease release on retire failed (likely already released)",
                        );
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
    // Publish a work-item status-change event only when the parent actually
    // transitioned (blocked → in_review). When the parent was already
    // `in_review` the status didn't change, so no broadcast is needed;
    // `ConflictResolutionSucceeded` (above) handles the activity-feed entry.
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
        raw_mergeable,
        raw_merge_state_status,
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
    use crate::work::{CreateChoreInput, CreateProductInput, WorkDb, WorkItem, WorkItemPatch};

    #[derive(Default)]
    struct RecordingPublisher {
        events: Mutex<Vec<(String, String, String)>>,
        typed_events: Mutex<Vec<(String, FrontendEvent)>>,
    }

    #[async_trait]
    impl ExecutionPublisher for RecordingPublisher {
        async fn publish(&self, _: &str, _: &str, _: &str, _: &str) {}
        async fn publish_work_item_changed(&self, product_id: &str, work_item_id: &str, reason: &str) {
            self.events
                .lock()
                .await
                .push((product_id.to_owned(), work_item_id.to_owned(), reason.to_owned()));
        }
        async fn publish_frontend_event_on_product(&self, product_id: &str, event: FrontendEvent) {
            self.typed_events.lock().await.push((product_id.to_owned(), event));
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
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name(name)
                    .autostart(false)
                    .build(),
            )
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

    fn chore_status(db: &WorkDb, id: &str) -> (TaskStatus, Option<String>) {
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
            raw_mergeable: String::new(),
            raw_merge_state_status: String::new(),
        }
    }

    /// A `PrStateChecker` that reports every PR as `Open`, so the
    /// `create_revision` create-time gate passes for the in-review chore
    /// fixtures these tests build. A conflicting PR is, by definition,
    /// still open — matching what `GhPrStateChecker` returns in production.
    fn open_checker() -> crate::work::FakePrStateChecker {
        crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open)
    }

    fn probe_with_labels(pr_url: &str, state: PrLifecycleState, labels: &[&str]) -> PrLifecycleProbe {
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
            raw_mergeable: String::new(),
            raw_merge_state_status: String::new(),
        }
    }

    /// New-model acceptance: when a revision fix vehicle is successfully spawned,
    /// the parent stays in `in_review` (Review column). The blocked state is only
    /// reached when there is no tractable fix vehicle (churn cap, create_revision
    /// failure, closed PR). See also `detection_blocks_parent_when_revision_fails`.
    #[tokio::test]
    async fn detection_keeps_parent_in_review_when_revision_spawns() {
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
        // transitioned == true because parent went in_review→blocked→in_review
        assert!(transitioned, "first detection must return true (state changed)");

        // Parent stays in Review — not blocked.
        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, TaskStatus::InReview);
        assert!(reason.is_none());

        // Event emitted is "conflict_revision_in_flight", not "blocked_merge_conflict".
        let events = pub_.events.lock().await.clone();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            (product.clone(), chore.clone(), "conflict_revision_in_flight".into())
        );

        // crz row exists and revision was spawned.
        let attempt = db.active_conflict_resolution_for_work_item(&chore).unwrap();
        assert!(attempt.is_some(), "crz attempt row must be present");
        let attempt = attempt.unwrap();
        assert_eq!(attempt.status, "pending");
        assert!(attempt.revision_task_id.is_some(), "revision must have been spawned");
    }

    /// When `create_revision` fails (parent PR closed/unmerged) or the churn cap
    /// pre-abandons the attempt, the parent DOES flip to `blocked: merge_conflict`
    /// so the human sees the card in Blocked.
    #[tokio::test]
    async fn detection_blocks_parent_when_revision_fails() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/10b";
        let (product, chore) = make_in_review(&db, "C-detect-fail", pr);
        let pub_ = Arc::new(RecordingPublisher::default());
        let closed = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::ClosedUnmerged);

        let transitioned = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &closed,
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;
        assert!(transitioned, "detection must return true (parent blocked)");

        // Parent is blocked since there is no active fix vehicle.
        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, TaskStatus::Blocked);
        assert_eq!(reason.as_deref(), Some("merge_conflict"));

        let events = pub_.events.lock().await.clone();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].2, "blocked_merge_conflict");

        // crz was abandoned (revision_create_failed).
        let attempts = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].status, "abandoned");
        assert_eq!(attempts[0].failure_reason.as_deref(), Some("revision_create_failed"),);
    }

    #[tokio::test]
    async fn detection_is_idempotent_on_repeated_probes() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/11";
        let (product, chore) = make_in_review(&db, "C-idem", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // First probe: conflict detected, revision spawned, parent stays in_review.
        let first = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;
        // Second probe with same base sha: UNIQUE collision on crz insert.
        // Existing crz has revision_task_id → upfront flip cleared back to
        // in_review by the collision path, but no net state change vs what
        // we already have.
        let second = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;
        assert!(first, "first probe must return true (state changed)");
        // Second probe: upfront flip still briefly goes to blocked then clears
        // back — returns true again because task_unblocked_for_revision=true.
        // The important invariant: parent ends up in_review, exactly one crz.
        let (status, _) = chore_status(&db, &chore);
        assert_eq!(
            status,
            TaskStatus::InReview,
            "parent must stay in_review after repeated probes"
        );
        let attempts = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
        assert_eq!(attempts.len(), 1, "same base sha must not stack crz rows");
        // Exactly one ConflictResolutionStarted typed event per probe.
        let started_count = pub_
            .typed_events
            .lock()
            .await
            .iter()
            .filter(|(_, ev)| matches!(ev, FrontendEvent::ConflictResolutionStarted { .. }))
            .count();
        assert!(started_count >= 1, "at least one ConflictResolutionStarted must fire");
        // At most two "conflict_revision_in_flight" events (one per probe), never
        // a "blocked_merge_conflict" since a fix vehicle is always in flight.
        let reasons: Vec<String> = pub_.events.lock().await.iter().map(|(_, _, r)| r.clone()).collect();
        assert!(
            reasons.iter().all(|r| r == "conflict_revision_in_flight"),
            "all work-item events must be conflict_revision_in_flight, got {reasons:?}",
        );
        let _ = second; // return value may be true or false; variant covered by the assertions above
    }

    /// New-model: parent was never blocked (revision spawned, stayed in_review).
    /// When the PR becomes clean, the crz attempt is retired and the signal
    /// cleared. The parent is already in_review — no status-change event fires.
    #[tokio::test]
    async fn resolution_retires_attempt_when_parent_was_in_review() {
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
        // Parent is in_review (revision spawned). Verify, then resolve.
        let (status_before, _) = chore_status(&db, &chore);
        assert_eq!(status_before, TaskStatus::InReview);

        let resolved = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
        assert!(resolved, "on_resolved must return true (attempt was retired)");

        // Parent still in_review — didn't change status.
        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, TaskStatus::InReview);
        assert!(reason.is_none());

        // No "merge_conflict_resolved" work-item event (parent didn't transition).
        let events = pub_.events.lock().await.clone();
        assert!(
            !events.iter().any(|(_, _, r)| r == "merge_conflict_resolved"),
            "merge_conflict_resolved must not fire when parent was already in_review",
        );

        // ConflictResolutionSucceeded typed event must fire.
        let typed = pub_.typed_events.lock().await.clone();
        assert!(
            typed
                .iter()
                .any(|(_, ev)| matches!(ev, FrontendEvent::ConflictResolutionSucceeded { .. })),
            "ConflictResolutionSucceeded must fire, got {typed:?}",
        );
    }

    /// Old-model compatibility: when the parent IS blocked (revision_create_failed,
    /// churn cap), on_resolved flips it back to in_review and emits
    /// "merge_conflict_resolved".
    #[tokio::test]
    async fn resolution_flips_blocked_parent_back_to_in_review() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/12b";
        let (product, chore) = make_in_review(&db, "C-resolve-blocked", pr);
        let pub_ = Arc::new(RecordingPublisher::default());
        let closed = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::ClosedUnmerged);

        // Drive into blocked via create_revision failure.
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &closed,
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;
        let (status_before, reason_before) = chore_status(&db, &chore);
        assert_eq!(status_before, TaskStatus::Blocked);
        assert_eq!(reason_before.as_deref(), Some("merge_conflict"));

        // Now manually install a running attempt (simulates legacy worker) and resolve.
        let attempt_id = install_running_attempt(&db, &product, &chore, pr, "lease-x");
        let resolved = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
        assert!(resolved);

        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, TaskStatus::InReview);
        assert!(reason.is_none());

        let events = pub_.events.lock().await.clone();
        assert!(
            events.iter().any(|(_, _, r)| r == "merge_conflict_resolved"),
            "merge_conflict_resolved must fire when parent was blocked, got {events:?}",
        );
        // Verify attempt was retired.
        let attempt_row = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
        assert_eq!(attempt_row.status, "succeeded");
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
        let r1 = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
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
        let r2 = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
        let r3 = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
        assert!(r2);
        assert!(!r3);
    }

    #[tokio::test]
    async fn cycle_conflict_resolve_conflict() {
        // Integration: conflict detected (revision in flight) → PR resolved →
        // conflict again (same base sha → UNIQUE collision, crz was succeeded,
        // no new active crz → parent flips to blocked this time).
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/14";
        let (product, chore) = make_in_review(&db, "C-cycle", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // 1st conflict: revision spawns, parent stays in_review.
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
        let (s, _) = chore_status(&db, &chore);
        assert_eq!(s, TaskStatus::InReview);

        // Resolve: PR goes clean, attempt retired, signal cleared.
        assert!(on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await);
        let (s, _) = chore_status(&db, &chore);
        assert_eq!(s, TaskStatus::InReview);

        // 2nd conflict: same base sha → UNIQUE collision. The previous crz is
        // now succeeded (no active crz). The upfront flip goes to blocked and
        // no revision is spawned (no fresh active crz to dispatch). Parent ends
        // up blocked because there is no fix vehicle.
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
        assert_eq!(status, TaskStatus::Blocked);
        assert_eq!(reason.as_deref(), Some("merge_conflict"));

        let reasons: Vec<String> = pub_.events.lock().await.iter().map(|(_, _, r)| r.clone()).collect();
        // 1st conflict → "conflict_revision_in_flight"
        // resolve    → no work-item event (parent was in_review)
        // 2nd conflict → "blocked_merge_conflict" (UNIQUE collision, no active crz)
        assert_eq!(
            reasons,
            vec![
                "conflict_revision_in_flight".to_owned(),
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
        assert_eq!(status, TaskStatus::Active);
        assert!(reason.is_none());
        assert!(pub_.events.lock().await.is_empty());
    }

    #[tokio::test]
    async fn resolution_skipped_when_human_moved_row_off_blocked() {
        // Use closed_checker so the parent actually ends up blocked
        // (revision_create_failed → no fix vehicle). The human then moves
        // the blocked row to `active` (manual override). on_resolved must
        // be a no-op because the active crz is abandoned (not pending/running).
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/16";
        let (product, chore) = make_in_review(&db, "C-human-2", pr);
        let pub_ = Arc::new(RecordingPublisher::default());
        let closed = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::ClosedUnmerged);
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &closed,
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;
        let (status_before, _) = chore_status(&db, &chore);
        assert_eq!(
            status_before,
            TaskStatus::Blocked,
            "sanity: closed_checker must cause blocked"
        );
        // Human moves the blocked row to `active`.
        db.update_work_item(
            &chore,
            WorkItemPatch {
                status: Some("active".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let before_count = pub_.events.lock().await.len();
        // on_resolved: abandoned crz → no active_conflict_resolution → clear_chore
        // WHERE guard misses (status='active') → no-op.
        let r = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
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
        async fn ensure_repo(&self, _origin: &str) -> anyhow::Result<crate::coordinator::CubeRepoHandle> {
            unreachable!("not used in conflict_watch tests")
        }
        async fn lease_workspace(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: bool,
            _: &[&str],
        ) -> anyhow::Result<crate::coordinator::CubeWorkspaceLease> {
            unreachable!("not used in conflict_watch tests")
        }
        async fn create_change(
            &self,
            _: &std::path::Path,
            _: &str,
        ) -> anyhow::Result<crate::coordinator::CubeChangeHandle> {
            unreachable!("not used in conflict_watch tests")
        }
        async fn goto_workspace(&self, _: &std::path::Path, _: u64) -> anyhow::Result<()> {
            unreachable!("not used in conflict_watch tests")
        }
        async fn release_workspace(&self, lease_id: &str) -> anyhow::Result<()> {
            self.releases.lock().await.push(lease_id.to_owned());
            if self.release_should_fail.load(std::sync::atomic::Ordering::SeqCst) {
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
        async fn force_release_lease(&self, _: &str, _: Option<&str>) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list_workspaces(&self) -> anyhow::Result<Vec<crate::coordinator::CubeWorkspaceStatus>> {
            Ok(Vec::new())
        }
        async fn list_repos(&self) -> anyhow::Result<Vec<crate::coordinator::CubeRepoSummary>> {
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
    async fn retire_with_running_attempt_releases_lease_and_emits_typed_event() {
        // Install a running attempt (different base sha than the probe) and drive
        // a resolve. The running crz is the most-recent active one so on_resolved
        // picks it up. Lease is released; ConflictResolutionSucceeded fires.
        // Parent was in_review the whole time (from on_conflict_detected which
        // spawned a revision), so no "merge_conflict_resolved" event fires.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/20";
        let (product, chore) = make_in_review(&db, "C-retire", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;
        // Install a running attempt (separate base-sha so no UNIQUE conflict).
        let attempt_id = install_running_attempt(&db, &product, &chore, pr, "lease-42");

        let cube = Arc::new(RecordingCubeClient::default());
        let resolved = on_resolved(
            &db,
            pub_.as_ref(),
            Some(cube.as_ref() as &dyn crate::coordinator::CubeClient),
            &candidate(&product, &chore, pr),
            &[],
            "",
            "",
        )
        .await;
        assert!(resolved, "retire path must return true");

        // Parent stays in_review — it was never blocked in new-model detection.
        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, TaskStatus::InReview);
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

        // Parent was in_review throughout → no "merge_conflict_resolved" event.
        let work_events = pub_.events.lock().await.clone();
        assert!(
            !work_events.iter().any(|(_, _, r)| r == "merge_conflict_resolved"),
            "merge_conflict_resolved must not fire when parent stayed in_review; got {work_events:?}",
        );

        let typed = pub_.typed_events.lock().await.clone();
        let succeeded_event = typed.iter().find(|(pid, ev)| {
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

    /// New: when the parent was blocked (old-model rows or create_revision failure)
    /// AND a running attempt exists, on_resolved flips the parent to in_review
    /// and emits merge_conflict_resolved.
    #[tokio::test]
    async fn retire_with_running_attempt_emits_resolved_when_parent_was_blocked() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/20b";
        let (product, chore) = make_in_review(&db, "C-retire-b", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // Use closed_checker to put parent in blocked state.
        let closed = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::ClosedUnmerged);
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &closed,
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;
        let (s, _) = chore_status(&db, &chore);
        assert_eq!(s, TaskStatus::Blocked);

        let attempt_id = install_running_attempt(&db, &product, &chore, pr, "lease-42");
        let cube = Arc::new(RecordingCubeClient::default());
        let resolved = on_resolved(
            &db,
            pub_.as_ref(),
            Some(cube.as_ref() as &dyn crate::coordinator::CubeClient),
            &candidate(&product, &chore, pr),
            &[],
            "",
            "",
        )
        .await;
        assert!(resolved);
        let (status, _) = chore_status(&db, &chore);
        assert_eq!(status, TaskStatus::InReview);
        assert_eq!(cube.releases.lock().await.as_slice(), ["lease-42"],);
        let work_events = pub_.events.lock().await.clone();
        assert!(
            work_events.iter().any(|(_, _, r)| r == "merge_conflict_resolved"),
            "merge_conflict_resolved must fire when parent was blocked, got {work_events:?}",
        );
        let attempt_row = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
        assert_eq!(attempt_row.status, "succeeded");
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
                Some((_, FrontendEvent::ConflictResolutionStarted { attempt_id, .. })) => attempt_id.clone(),
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
            "",
            "",
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

    /// Reconciliation path (T791/T898 scenario): parent is in `blocked: merge_conflict`
    /// but an active revision is already in flight. The next CONFLICTING probe should
    /// flip the parent BACK to `in_review` without spawning a second revision.
    #[tokio::test]
    async fn rearm_reconciles_blocked_parent_when_revision_is_in_flight() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/20r";
        let (product, chore) = make_in_review(&db, "C-rearm-reconcile", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // Simulate the pre-model-change state: parent is blocked AND a revision
        // exists (T898-style). Manually flip to blocked, insert a crz, create a
        // revision, stamp the crz's revision_task_id.
        db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
        let attempt = db
            .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
                product_id: product.clone(),
                work_item_id: chore.clone(),
                pr_url: pr.into(),
                pr_number: 20,
                head_branch: "feature".into(),
                base_branch: "main".into(),
                base_sha_at_trigger: Some("abc123".into()),
                head_sha_before: Some("head456".into()),
            })
            .unwrap()
            .expect("fresh insert");
        // Stamp a fake revision_task_id to simulate T898 being active.
        db.set_conflict_resolution_revision_task_id(&attempt.id, "task_fake_revision")
            .unwrap();
        let (s, _) = chore_status(&db, &chore);
        assert_eq!(s, TaskStatus::Blocked, "sanity: parent must be blocked before probe");

        // Now fire on_conflict_detected for the same PR (still CONFLICTING).
        // The re-arm path should find the active revision and reconcile.
        let reconciled = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;

        assert!(reconciled, "reconciliation must return true (state changed)");
        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(
            status,
            TaskStatus::InReview,
            "parent must be back in_review after reconcile"
        );
        assert!(reason.is_none());

        // Event emitted is "conflict_revision_in_flight".
        let events = pub_.events.lock().await.clone();
        assert!(
            events.iter().any(|(_, _, r)| r == "conflict_revision_in_flight"),
            "conflict_revision_in_flight event must fire during reconcile, got {events:?}",
        );
        // No second revision was spawned (task_fake_revision is still the only one).
        let all_crz = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
        assert_eq!(all_crz.len(), 1, "reconcile must not insert a new crz");
    }

    #[tokio::test]
    async fn retire_is_idempotent_on_repeated_probes_with_active_attempt() {
        // Second sweep over a row already retired must NOT re-emit events nor
        // re-release the cube lease. Use closed_checker to put the parent in
        // blocked state (create_revision fails → no fix vehicle), then install
        // a running attempt as the lone active crz. First on_resolved retires
        // that attempt; second finds no active crz → clean no-op.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/21";
        let (product, chore) = make_in_review(&db, "C-retire-idem", pr);
        let pub_ = Arc::new(RecordingPublisher::default());
        let closed = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::ClosedUnmerged);

        // Use closed_checker: parent goes blocked (create_revision fails, crz abandoned).
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &closed,
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;
        // Install a running attempt as the lone active crz.
        install_running_attempt(&db, &product, &chore, pr, "lease-99");

        let cube = Arc::new(RecordingCubeClient::default());
        let first = on_resolved(
            &db,
            pub_.as_ref(),
            Some(cube.as_ref() as &dyn crate::coordinator::CubeClient),
            &candidate(&product, &chore, pr),
            &[],
            "",
            "",
        )
        .await;
        let second = on_resolved(
            &db,
            pub_.as_ref(),
            Some(cube.as_ref() as &dyn crate::coordinator::CubeClient),
            &candidate(&product, &chore, pr),
            &[],
            "",
            "",
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
            "",
            "",
        )
        .await;
        assert!(resolved, "retire transitions must still report success");
        let attempt_row = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
        assert_eq!(attempt_row.status, "succeeded");
        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, TaskStatus::InReview);
        assert!(reason.is_none());
    }

    #[tokio::test]
    async fn detection_emits_started_event_reuses_existing_row_on_same_base_sha() {
        // When on_conflict_detected is called a second time for the same
        // base sha while a revision is in flight, the pre-flight early-exit
        // fires and no new events are emitted (pure no-op). The first call
        // created the attempt and emitted ConflictResolutionStarted; that's
        // the authoritative event. Only one crz row must exist.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/23";
        let (product, chore) = make_in_review(&db, "C-detect-evt", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // First call — creates the attempt, spawns revision, parent stays in_review.
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

        // Second call: same base sha, revision already in flight → pre-flight
        // early-exit. Returns false (no-op), no new typed events.
        let second = on_conflict_detected(
            &db,
            pub_.as_ref(),
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;
        assert!(!second, "second probe with active revision must be a no-op");

        // Only one crz row; only one started event.
        let all_started: Vec<_> = pub_
            .typed_events
            .lock()
            .await
            .iter()
            .filter(|(_, ev)| matches!(ev, FrontendEvent::ConflictResolutionStarted { .. }))
            .cloned()
            .collect();
        assert_eq!(all_started.len(), 1, "no second started event from idempotent no-op");
        if let FrontendEvent::ConflictResolutionStarted { attempt_id: a, .. } = &all_started[0].1 {
            assert_eq!(a, &first_attempt_id);
        }
        let crz_count = db
            .list_conflict_resolutions(None, &[], Some(&chore), None)
            .unwrap()
            .len();
        assert_eq!(crz_count, 1, "same base sha must not create a second crz row");
        let _ = (product, first_attempt_id); // silence unused warnings
    }

    #[tokio::test]
    async fn detection_inserts_attempt_and_emits_started_event() {
        // on_conflict_detected inserts the conflict_resolution attempt and emits
        // ConflictResolutionStarted in the same call. Parent stays in_review
        // when revision spawns (no pre-wiring needed for on_resolved to fire).
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

        let attempt = db.active_conflict_resolution_for_work_item(&chore).unwrap();
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
        assert_eq!(status, TaskStatus::InReview, "row stays where it was");
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
        assert_eq!(status, TaskStatus::InReview);
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
        assert_eq!(status, TaskStatus::InReview);
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

        // Detect conflict with maintenance enabled: new model keeps parent
        // in_review (revision spawned). Then disable maintenance and assert
        // the retire path is a no-op.
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await;
        // New-model: parent stays in_review after detection (revision in flight).
        let (status_before, _) = chore_status(&db, &chore);
        assert_eq!(status_before, TaskStatus::InReview);
        let before = pub_.events.lock().await.len();
        set_product_auto_pr_maintenance(&db_path, &product, false);

        let r = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
        assert!(!r, "opted-out product must not retire automatically");
        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(status, TaskStatus::InReview);
        assert!(reason.is_none());
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
                assert_eq!(t.status, TaskStatus::Blocked);
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
        // `revision_task_id`, creates NO bespoke conflict_resolution execution,
        // and leaves the parent in `in_review` (new-model parent-state).
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

        // Parent stays in_review — the revision card is the Doing card.
        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(
            status,
            TaskStatus::InReview,
            "parent must stay in Review while revision is in flight"
        );
        assert!(reason.is_none());

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
        assert_eq!(revision.kind, TaskKind::Revision);
        assert_eq!(revision.parent_task_id.as_deref(), Some(chore.as_str()));
        assert_eq!(revision.created_via, format!("merge-conflict:{}", attempt.id));
        assert_eq!(revision.description, "Resolve merge conflict against main");

        // No bespoke conflict_resolution execution: the revision rides the
        // reconcile loop's revision_implementation dispatch instead.
        let ready = db.list_ready_executions().unwrap();
        assert!(
            !ready.iter().any(|e| e.kind == ExecutionKind::ConflictResolution),
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

        let attempts = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
        assert_eq!(attempts.len(), 1, "same base sha must not stack attempts");
        let revision_backed = attempts.iter().filter(|r| r.revision_task_id.is_some()).count();
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
        assert_eq!(fourth.failure_reason.as_deref(), Some("churn_threshold_exceeded"),);
        assert!(
            fourth.revision_task_id.is_none(),
            "churn-abandoned attempt must spawn no revision",
        );
        // Churn cap = no fix vehicle → parent must be blocked (human-attention terminal).
        let (status, reason) = chore_status(&db, &chore);
        assert_eq!(
            status,
            TaskStatus::Blocked,
            "churn cap exhausted: parent must be blocked"
        );
        assert_eq!(reason.as_deref(), Some("merge_conflict"));
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
        let closed = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::ClosedUnmerged);

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
        assert_eq!(status, TaskStatus::Blocked);
        assert_eq!(reason.as_deref(), Some("merge_conflict"));

        let attempts = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].status, "abandoned");
        assert_eq!(attempts[0].failure_reason.as_deref(), Some("revision_create_failed"),);
        assert!(attempts[0].revision_task_id.is_none());
    }
}
