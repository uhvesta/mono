//! Detection-trigger pipeline for CI-failure handling on `in_review`
//! PRs (`tools/boss/docs/designs/merge-conflict-handling-in-review.md`
//! §"CI worker spawn and the fix-CI playbook" / Phase 8 #22).
//!
//! Two entry points, both invoked from `merge_poller::sweep_one`:
//!
//!   - [`on_ci_failure_detected`] — fired when the probe reports an
//!     open, mergeable PR whose required checks include at least one
//!     definitive failure. Flips the parent `tasks` row from
//!     `in_review` to `blocked: ci_failure` (or
//!     `ci_failure_exhausted` when the per-PR budget is spent),
//!     inserts a `ci_remediations` row, and emits a typed
//!     `FrontendEvent::CiRemediationStarted` (or
//!     `CiRemediationExhausted`).
//!
//!   - [`on_ci_resolved`] — fired when the probe reports a previously
//!     CI-blocked PR back at green (or carrying no failing required
//!     checks). Flips the parent back to `in_review`, clears the
//!     scalar / side-table CI signals, and flips the matching
//!     `ci_remediations` row to `succeeded` if one exists.
//!
//! Both transitions are idempotent: a repeat probe finds the row
//! already in the target state and writes nothing. Worker spawn and
//! the `CiLogReader` traits ship in Phase 9; this module owns the
//! Phase 8 detection + retire seams.
//!
//! Composed ordering (design §Q7): the dispatch site (the merge
//! poller's `sweep_one`) already routes a conflicting PR exclusively
//! to `conflict_watch`, so this module is only ever invoked when
//! `mergeability=Clean`. But an active higher-priority attempt — an
//! `auto-rebase` or `conflict_resolutions` row — can still be
//! covering the same PR (it cleared the conflict moments ago and
//! hasn't retired yet). `on_ci_failure_detected` defers in that case.

use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
use boss_protocol::TaskKind;
use boss_protocol::{
    CREATED_VIA_CI_FIX_PREFIX, CreateAttentionItemInput, CreateRevisionInput, ExecutionKind, ExecutionStatus,
    FrontendEvent,
};
use serde::Serialize;

use crate::blocking_signal::{self, SignalKind};
use crate::coordinator::ExecutionPublisher;
use crate::merge_poller::{PrLifecycleProbe, RequiredCheckFailure, parse_pr_number, pr_labels_opt_out};
use crate::work::{
    CiRemediation, CiRemediationInsertInput, CreateExecutionInput, PendingMergeCheck, PrStateChecker,
    StrandedCiRemediationAttempt, WorkDb,
};

/// Pre-spawn classification (design §Q4 "pre-triage"): if every failure
/// has `conclusion ∈ {STARTUP_FAILURE, CANCELLED}` (engine-discernible
/// infra signals) the attempt is a `retrigger` — the engine doesn't
/// need to read the log, doesn't burn a fix-budget slot. Everything
/// else routes to `fix`, where the worker reads the log and (per the
/// reconciled 2026-05-17 design call) rebases onto base HEAD before
/// attempting any code change.
///
/// Pulled out as a free function so the unit tests can drive every
/// conclusion-set permutation without standing up a publisher / DB.
/// Returns `"retrigger"` or `"fix"` — the exact strings stored in
/// `ci_remediations.attempt_kind`.
pub fn classify_pre_triage(failures: &[RequiredCheckFailure]) -> &'static str {
    if failures.is_empty() {
        // Defensive: an empty failure set isn't an actionable trigger,
        // but the caller already filters on this — return `fix` so a
        // future caller that hands us an empty slice still produces a
        // budgeted attempt rather than silently retriggering.
        return "fix";
    }
    let all_infra = failures
        .iter()
        .all(|f| matches!(f.conclusion.as_str(), "STARTUP_FAILURE" | "CANCELLED"));
    if all_infra { "retrigger" } else { "fix" }
}

/// Buckets for the Phase 12 #39 never-starts soft alert. The engine
/// emits a `warn`-level log when CI has been `InFlight` continuously
/// for at least `WARN_THRESHOLD_SECS`, and a typed soft alert (plus a
/// louder log line) when the duration crosses `ALERT_THRESHOLD_SECS`.
const NEVER_STARTS_WARN_THRESHOLD_SECS: i64 = 30 * 60;
const NEVER_STARTS_ALERT_THRESHOLD_SECS: i64 = 2 * 60 * 60;

/// Unified opt-out gate. Mirrors `conflict_watch::auto_pr_maintenance_disabled`;
/// the design (Phase 6 #18 / §Q7) requires both auto-remediation
/// paths to honour the same per-product flag and per-PR label.
fn auto_pr_maintenance_disabled(work_db: &WorkDb, candidate: &PendingMergeCheck, labels: &[String]) -> bool {
    if pr_labels_opt_out(labels) {
        tracing::debug!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            "ci_watch: PR labelled with opt-out; skipping",
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
                "ci_watch: product opted out of auto_pr_maintenance; skipping",
            );
            true
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                product_id = %candidate.product_id,
                ?err,
                "ci_watch: failed to read auto_pr_maintenance_enabled; treating as enabled",
            );
            false
        }
    }
}

/// JSON-encodable snapshot of one failing check; the wire shape of
/// each entry in `ci_remediations.failed_checks`. Kept here rather
/// than on the protocol crate because it's an engine-internal
/// detection-time record — the protocol `CiRemediation` exposes the
/// list as a raw `failed_checks: String` so the schema can roll
/// forward without bumping the wire type.
#[derive(Debug, Clone, Serialize)]
struct FailedCheckRecord<'a> {
    name: &'a str,
    conclusion: &'a str,
    target_url: &'a str,
    provider: &'a str,
    provider_job_id: Option<&'a str>,
}

/// Attention kind used when CI remediation exhausts its attempt budget.
const CI_REMEDIATION_EXHAUSTED_ATTENTION_KIND: &str = "ci_remediation_exhausted";

/// Create a work-item-scoped attention item signalling that automated CI
/// remediation gave up, and emit [`FrontendEvent::AttentionItemCreated`]
/// so the UI surfaces it immediately. Best-effort: filing errors are
/// logged and swallowed so the caller's main state transition still
/// succeeds.
///
/// `failing_check_names` is the display list of check names included in
/// the attention body. Pass `&[]` when the names are not available (e.g.
/// merge-queue rebounce path where the failing SHA belongs to the
/// synthetic merge commit rather than the PR head).
async fn emit_exhausted_attention(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    product_id: &str,
    work_item_id: &str,
    pr_url: &str,
    used: i64,
    budget: i64,
    failing_check_names: &[&str],
) {
    let checks_detail = if failing_check_names.is_empty() {
        String::new()
    } else {
        let list = failing_check_names
            .iter()
            .map(|n| format!("- `{n}`"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("\n\n**Failing checks:**\n{list}")
    };
    let body = format!(
        "Auto-CI remediation exhausted its attempt budget ({used}/{budget}) on PR {pr_url} \
         and will not spawn further fix revisions. Manual intervention is required to \
         resolve the failing checks and re-queue the PR.{checks_detail}"
    );
    let title = format!("Auto-CI remediation exhausted ({used}/{budget} attempts)");
    match work_db.create_attention_item(CreateAttentionItemInput {
        execution_id: None,
        work_item_id: Some(work_item_id.to_owned()),
        kind: CI_REMEDIATION_EXHAUSTED_ATTENTION_KIND.to_owned(),
        status: None,
        title,
        body_markdown: body,
        resolved_at: None,
    }) {
        Ok(item) => {
            publisher
                .publish_frontend_event_on_product(product_id, FrontendEvent::AttentionItemCreated { item })
                .await;
        }
        Err(err) => {
            tracing::warn!(
                work_item_id,
                pr_url,
                used,
                budget,
                ?err,
                "ci_watch: failed to file ci_remediation_exhausted attention item",
            );
        }
    }
}

/// Detection-side entry point. Returns `true` when the parent
/// transitioned to `blocked: ci_failure` (or
/// `blocked: ci_failure_exhausted`) on this call. All paths that
/// don't transition — opt-out, suppression, higher-priority attempt
/// active, WHERE-guard miss, DB error — return `false` and log at
/// the appropriate level.
///
/// `failures` is the list the probe collected from `statusCheckRollup`
/// (design §Q1's predicate); it is also persisted as the row's
/// `failed_checks` JSON for the worker prompt.
///
/// Phase 4 cutover (design Q1/Q5): on a genuinely-new `fix`-kind attempt
/// the fix vehicle is now an **engine-triggered revision** (`parent =
/// chore`, `created_via = "ci-fix:<crm_id>"`) created via the shared
/// `create_revision` gate, rather than a bespoke `ci_remediation`
/// execution. `retrigger` produces no commit, so it stays on the bespoke
/// dispatch (design Q6). The CI budget is enforced *before* create
/// (unchanged): an exhausted PR flips to `ci_failure_exhausted` and never
/// reaches the revision-spawn path. `pr_checker` supplies the create-time
/// gate's PR-state probe (`&StaticPrStateChecker(Open)` in production —
/// the poller has just observed this PR open at clean mergeability — and a
/// fake in tests), reusing `assert_parent_revisable` (R4).
pub async fn on_ci_failure_detected(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    pr_checker: &dyn PrStateChecker,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
    failures: &[RequiredCheckFailure],
) -> bool {
    if failures.is_empty() {
        // Defensive — the dispatch site already filtered on Failing,
        // but if a future caller hands us an empty set we should not
        // flip the row.
        return false;
    }
    if auto_pr_maintenance_disabled(work_db, candidate, &probe.labels) {
        return false;
    }
    // §Q7 composed ordering: an active conflict-resolution attempt
    // (or auto-rebase escalation) for this PR owns the slot until
    // terminal. CI watch defers; the next sweep re-evaluates once the
    // higher-priority attempt clears.
    match work_db.has_active_rebase_attempt_for_pr(&candidate.pr_url) {
        Ok(true) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "ci_watch: rebase attempt active; deferring ci_failure flip",
            );
            return false;
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "ci_watch: failed to check rebase attempt; deferring",
            );
            return false;
        }
    }
    match work_db.active_conflict_resolution_for_work_item(&candidate.work_item_id) {
        Ok(Some(_)) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "ci_watch: conflict resolution attempt active; deferring ci_failure flip",
            );
            return false;
        }
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "ci_watch: failed to check active conflict_resolutions; deferring",
            );
            return false;
        }
    }

    // Pre-flight (mirrors conflict_watch::on_conflict_detected): a fix revision
    // is already in flight for this work item — an idempotent re-probe while the
    // CI is still red, or a row blocked before the in_review model shipped.
    // Re-arm the side-table signal and either reconcile a still-`blocked` parent
    // back to `in_review` or no-op for an already-`in_review` parent, without
    // churning the flip / insert / budget path on every sweep.
    if let Ok(Some(active)) = work_db.active_ci_remediation_for_work_item(&candidate.work_item_id)
        && active.revision_task_id.is_some()
    {
        if work_db
            .rearm_blocked_ci_failure_signal(&candidate.work_item_id)
            .unwrap_or(false)
        {
            // Parent is still `blocked: ci_failure` with an active revision —
            // reconcile it back to `in_review`; the revision card in Doing is
            // the user-visible signal.
            let reconciled = blocking_signal::reconcile_blocked_parent_with_revision(
                work_db,
                SignalKind::CiFailure,
                candidate,
                &active.id,
            );
            if reconciled {
                publisher
                    .publish_work_item_changed(&candidate.product_id, &candidate.work_item_id, "ci_revision_in_flight")
                    .await;
            }
            return reconciled;
        }
        // Parent is `in_review` (or human-moved): idempotent probe. Keep the
        // in-flight signal armed so `maybe_clear_blocked` fires on green.
        let _ = work_db.record_ci_failure_in_flight(&candidate.work_item_id, &active.id);
        return false;
    }

    // Secondary pre-flight: gate on any ci-fix revision that is still
    // in flight (status `todo`, `active`, or `blocked`). The primary gate
    // above (ci_remediations status IN ('pending', 'running')) can be
    // bypassed when `try_retire_cleared_blocking_signal` marks the row
    // `succeeded` prematurely — specifically when the originally-failing
    // checks are no longer in the failure set (e.g. a re-triggered flaky
    // check now passes) while the revision worker is still running and has
    // not pushed a fix commit. Without this guard, a new revision can spawn
    // concurrently with the prior one, wasting the attempt budget and racing
    // the same PR branch (observed: T1437→T1438→T1439 firing 4–6 min apart,
    // shorter than one worker+CI cycle — issue T1431 / PR #1404).
    match work_db.has_in_flight_ci_fix_revision(&candidate.work_item_id) {
        Ok(true) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "ci_watch: ci-fix revision still in flight (worker active or blocked); \
                 deferring to prevent overlapping attempts",
            );
            return false;
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "ci_watch: failed to check in-flight ci-fix revision; deferring",
            );
            return false;
        }
    }

    // The head sha is the discriminator for both the suppression
    // table and the `ci_remediations` unique key. Without it we can't
    // de-duplicate probes for the same failing head, so we leave the
    // row alone — the next sweep with a populated `headRefOid` will
    // pick it up.
    let Some(head_sha) = probe.head_ref_oid.as_deref() else {
        tracing::debug!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            "ci_watch: probe missing headRefOid; cannot key the attempt — deferring",
        );
        return false;
    };

    // Manual-override suppression (design §Q5): the user pulled the
    // chore out of `blocked: ci_failure` themselves. Honour that for
    // the same head sha; a new push invalidates the suppression
    // automatically by changing the key.
    match work_db.is_ci_failure_suppressed(&candidate.work_item_id, head_sha) {
        Ok(true) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                head_sha,
                "ci_watch: ci_failure suppression active for this head_sha; skipping",
            );
            return false;
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "ci_watch: failed to read suppression table; continuing",
            );
        }
    }

    // Budget check (design §Q3). A used >= budget here means we've
    // already burned the allotment for this PR — flip the parent to
    // `ci_failure_exhausted` and emit the typed event, but do not
    // insert an attempt row.
    let used = work_db.get_ci_attempts_used(&candidate.work_item_id).unwrap_or(0);
    let budget = work_db.effective_ci_budget(&candidate.work_item_id).unwrap_or(3);
    if used >= budget {
        match work_db.mark_chore_blocked_ci_failure_exhausted(&candidate.work_item_id, &candidate.pr_url) {
            Ok(Some(_)) => {
                publisher
                    .publish_work_item_changed(
                        &candidate.product_id,
                        &candidate.work_item_id,
                        "blocked_ci_failure_exhausted",
                    )
                    .await;
                publisher
                    .publish_frontend_event_on_product(
                        &candidate.product_id,
                        FrontendEvent::CiRemediationExhausted {
                            product_id: candidate.product_id.clone(),
                            work_item_id: candidate.work_item_id.clone(),
                            pr_url: candidate.pr_url.clone(),
                            attempts_used: used,
                            budget,
                        },
                    )
                    .await;
                let check_names: Vec<&str> = failures.iter().map(|f| f.name.as_str()).collect();
                emit_exhausted_attention(
                    work_db,
                    publisher,
                    &candidate.product_id,
                    &candidate.work_item_id,
                    &candidate.pr_url,
                    used,
                    budget,
                    &check_names,
                )
                .await;
                tracing::info!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    used,
                    budget,
                    "ci_watch: budget exhausted; parent flipped to blocked: ci_failure_exhausted",
                );
                return true;
            }
            Ok(None) => {
                tracing::debug!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    "ci_watch: ci_failure_exhausted WHERE guard missed",
                );
                return false;
            }
            Err(err) => {
                tracing::warn!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    ?err,
                    "ci_watch: failed to flip row to blocked: ci_failure_exhausted",
                );
                return false;
            }
        }
    }

    // Pre-spawn classification (design §Q4 "pre-triage"): if every
    // failure is `STARTUP_FAILURE` or `CANCELLED` we choose
    // `retrigger`; otherwise `fix`. Retriggers don't consume budget.
    let attempt_kind = classify_pre_triage(failures);
    let consumes_budget: i64 = if attempt_kind == "fix" { 1 } else { 0 };

    let failed_checks_json = encode_failed_checks(failures);
    let pr_number = parse_pr_number(&candidate.pr_url).unwrap_or(0);

    // Best-effort attempt insert. The unique key
    // (work_item_id, head_sha, attempt_kind) is the idempotency lock —
    // a second probe for the same triplet finds the row already
    // present and `INSERT OR IGNORE` updates zero rows; we still want
    // to flip the parent to `blocked: ci_failure` if it isn't already
    // there (e.g. the engine restarted mid-cycle).
    let insert_result = work_db.insert_ci_remediation(CiRemediationInsertInput {
        product_id: candidate.product_id.clone(),
        work_item_id: candidate.work_item_id.clone(),
        pr_url: candidate.pr_url.clone(),
        pr_number,
        head_branch: probe.head_ref_name.clone().unwrap_or_default(),
        head_sha_at_trigger: head_sha.to_owned(),
        attempt_kind: attempt_kind.to_owned(),
        consumes_budget,
        failed_checks: failed_checks_json,
        failure_kind: "pr_branch_ci".to_owned(),
        before_commit_sha: None,
    });
    let attempt = match insert_result {
        Ok(row) => row,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "ci_watch: failed to insert ci_remediations row",
            );
            None
        }
    };

    let attempt_id = attempt.as_ref().map(|a| a.id.clone());

    // The CI rollup has now flipped to `Failing`, which means the
    // never-starts observation (tracked while we were in `InFlight`)
    // is no longer the relevant signal — clear any leftover rows so
    // the next time the same PR sits in InFlight we re-key from
    // scratch. Best-effort.
    if let Err(err) = work_db.clear_ci_inflight_observations(&candidate.work_item_id) {
        tracing::debug!(
            work_item_id = %candidate.work_item_id,
            ?err,
            "ci_watch: failed to clear inflight observations on Failing transition",
        );
    }

    let task_result =
        work_db.mark_chore_blocked_ci_failure(&candidate.work_item_id, &candidate.pr_url, attempt_id.as_deref());
    let task_transitioned = match task_result {
        Ok(Some(_)) => true,
        Ok(None) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "ci_watch: WHERE guard missed; row already blocked or manually moved",
            );
            false
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "ci_watch: failed to flip row to blocked: ci_failure",
            );
            return false;
        }
    };

    // Phase 4 cutover (design Q1/Q5): on a genuinely-new live `fix` attempt
    // the fix vehicle is an engine-triggered revision, NOT a bespoke
    // `ci_remediation` execution. The `revision_task_id` soft-FK is the
    // idempotency latch (a same-triplet repeat probe hits `Ok(None)` on the
    // insert above, so `attempt` is `None` and this branch is skipped) and
    // the marker that hides the row from the dormant `ci_remediation` rescue
    // path. Decoupled from `task_transitioned` (mirrors
    // `conflict_watch::on_conflict_detected`) so a fresh failing head_sha
    // that lands while the parent is already `blocked: ci_failure` still gets
    // a revision rather than stranding into the bespoke rescue dispatch.
    // `retrigger` produces no commit (design Q6) — it stays on the bespoke
    // path handled in the `task_transitioned` block below. Budget exhaustion
    // was already handled above (no insert, no attempt), so an exhausted PR
    // never reaches here.
    // #1007 parent-state model, now shared with the conflict path via
    // [`crate::blocking_signal`]: on a successful `fix`-revision spawn, clear
    // the upfront `blocked: ci_failure` flip back to `in_review` and record the
    // in-flight signal, so the parent stays in the Review column while the
    // revision runs in Doing.
    let mut task_unblocked_for_revision = false;
    if let Some(ref a) = attempt
        && a.attempt_kind == "fix"
        && a.status == "pending"
        && a.revision_task_id.is_none()
        && maybe_spawn_ci_revision(work_db, publisher, pr_checker, candidate, failures, a).await
    {
        task_unblocked_for_revision =
            blocking_signal::unblock_for_revision(work_db, SignalKind::CiFailure, candidate, &a.id);
    }
    // If the spawn was refused (create_revision gate), the attempt is
    // abandoned and the parent stays `blocked: ci_failure` — the
    // human-attention terminal.

    // (The "parent already blocked with an active revision" reconcile case is
    // handled by the pre-flight early-exit above; here `task_unblocked_for_revision`
    // is set only by a fresh-attempt spawn.)
    let task_changed = task_transitioned || task_unblocked_for_revision;
    if task_changed {
        // Bump the budget counter when we created a fix-kind attempt — the
        // design (§Q3) says the counter increments when "a fix attempt
        // actually progresses past the worker's go/no-go." The flip may have
        // been cleared back to `in_review` for an in-flight revision, but a fix
        // attempt still progressed, so the bump is keyed off the attempt, not
        // the parent's terminal status.
        if attempt.is_some()
            && attempt_kind == "fix"
            && let Err(err) = work_db.increment_ci_attempts_used(&candidate.work_item_id)
        {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                ?err,
                "ci_watch: failed to increment ci_attempts_used",
            );
        }
        // Parent stays in Review while the revision runs
        // (`ci_revision_in_flight`); it surfaces in Blocked
        // (`blocked_ci_failure`) only when there is no fix vehicle.
        let change_reason = if task_unblocked_for_revision {
            "ci_revision_in_flight"
        } else {
            "blocked_ci_failure"
        };
        publisher
            .publish_work_item_changed(&candidate.product_id, &candidate.work_item_id, change_reason)
            .await;
        if let Some(attempt) = attempt.as_ref() {
            // `retrigger` attempts produce no commit, so they stay on the
            // bespoke `ci_remediation` execution kind (design Q6): create a
            // `ready` execution and kick the scheduler. `fix` attempts ride
            // the engine-triggered revision spawned above and must NOT get a
            // bespoke execution (the cutover). The unique key already gated
            // us, so a second probe with the same triplet sees
            // `attempt = None` and skips this branch entirely.
            if attempt.attempt_kind == "retrigger" {
                match work_db.create_execution(
                    CreateExecutionInput::builder()
                        .work_item_id(candidate.work_item_id.clone())
                        .kind(ExecutionKind::CiRemediation)
                        .status(ExecutionStatus::Ready)
                        .build(),
                ) {
                    Ok(_) => publisher.kick_scheduler(),
                    Err(err) => {
                        tracing::warn!(
                            work_item_id = %candidate.work_item_id,
                            attempt_id = %attempt.id,
                            ?err,
                            "ci_watch: failed to create ci_remediation retrigger execution; worker will not be dispatched",
                        );
                    }
                }
            }
            publisher
                .publish_frontend_event_on_product(
                    &candidate.product_id,
                    FrontendEvent::CiRemediationStarted {
                        product_id: candidate.product_id.clone(),
                        work_item_id: candidate.work_item_id.clone(),
                        attempt_id: attempt.id.clone(),
                        pr_url: candidate.pr_url.clone(),
                        attempt_kind: attempt.attempt_kind.clone(),
                    },
                )
                .await;
        }
        tracing::info!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            head_sha,
            attempt_kind,
            failures = failures.len(),
            task_transitioned,
            task_unblocked_for_revision,
            "ci_watch: CI failure detected; remediation flow ran",
        );
        true
    } else {
        false
    }
}

/// Build the short, one-line revision card title from the failing checks
/// (design Q3 / R5: generated from check *names*, never the log body).
/// Shows up to the first three check names, e.g.
/// `Fix failing CI: ci/test, ci/lint`; with more than three it appends a
/// `(+N more)` tail so the Review-lane card stays one line. The long worker
/// directive (log excerpt, failed-check table, rebase recipe) is injected at
/// dispatch by `compose_revision_directive`, keyed off the `ci-fix:`
/// `created_via` (Phase 2).
fn ci_revision_description(failures: &[RequiredCheckFailure]) -> String {
    const MAX_NAMES: usize = 3;
    let names: Vec<&str> = failures
        .iter()
        .map(|f| f.name.as_str())
        .filter(|n| !n.is_empty())
        .collect();
    if names.is_empty() {
        return "Fix failing CI".to_owned();
    }
    let shown = names.iter().take(MAX_NAMES).copied().collect::<Vec<_>>().join(", ");
    if names.len() > MAX_NAMES {
        format!("Fix failing CI: {shown} (+{} more)", names.len() - MAX_NAMES)
    } else {
        format!("Fix failing CI: {shown}")
    }
}

/// Create the engine-triggered revision that delivers the CI fix and stamp
/// the trigger-ledger row's `revision_task_id` back-pointer (design
/// Q1/Q2/Q5). Mirror of `conflict_watch::maybe_spawn_conflict_revision`.
///
/// `attempt` is the just-inserted, live (`pending`), `fix`-kind
/// `ci_remediations` row. On success the reconcile loop picks up the new
/// `kind=revision` task and dispatches a `revision_implementation` execution
/// into the chain root's warm workspace; the `ci-fix:` provenance makes
/// `runner.rs` inject the CI log excerpt + failed-check fragment into the
/// worker directive (Phase 2). On failure — almost always the create-time
/// gate (`assert_parent_revisable`, R4) refusing a parent whose PR has since
/// merged/closed — the ledger row is marked `abandoned` so it never strands
/// as a `pending` attempt with no fix vehicle. The parent stays
/// `blocked: ci_failure`; the poller's merged/closed handling reconciles it
/// on a later sweep.
async fn maybe_spawn_ci_revision(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    pr_checker: &dyn PrStateChecker,
    candidate: &PendingMergeCheck,
    failures: &[RequiredCheckFailure],
    attempt: &CiRemediation,
) -> bool {
    let description = ci_revision_description(failures);
    let created_via = format!("{CREATED_VIA_CI_FIX_PREFIX}{}", attempt.id);

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
                "ci_watch: create_revision failed (parent likely no longer revisable); abandoning attempt",
            );
            if let Err(abandon_err) = work_db.mark_ci_remediation_abandoned(&attempt.id, "revision_create_failed") {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    ?abandon_err,
                    "ci_watch: failed to abandon attempt after create_revision failure",
                );
            }
            // Spawn refused (parent no longer revisable). Parent stays
            // `blocked: ci_failure` — the human-attention terminal.
            return false;
        }
    };

    // Stamp the reverse link. This is the idempotency latch (repeat probes at
    // the same head sha find it set and skip) and the marker that tells the
    // dormant rescue path to leave this row alone.
    match work_db.set_ci_remediation_revision_task_id(&attempt.id, &revision.id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            tracing::warn!(
                attempt_id = %attempt.id,
                revision_task_id = %revision.id,
                "ci_watch: attempt row vanished before revision_task_id could be stamped",
            );
        }
        Err(err) => {
            tracing::warn!(
                attempt_id = %attempt.id,
                revision_task_id = %revision.id,
                ?err,
                "ci_watch: failed to stamp revision_task_id; revision will still run",
            );
        }
    }

    tracing::info!(
        work_item_id = %candidate.work_item_id,
        pr_url = %candidate.pr_url,
        attempt_id = %attempt.id,
        revision_task_id = %revision.id,
        "ci_watch: spawned engine-triggered revision for CI failure",
    );

    // Nudge the scheduler so the reconcile loop dispatches the revision's
    // `revision_implementation` execution promptly.
    publisher.kick_scheduler();
    true
}

/// Entry point for merge-queue rebounce detection.
///
/// Called from `merge_poller::sweep` when a `RemovedFromMergeQueueEvent`
/// with `reason=FAILED_CHECKS` is detected for an `in_review` PR.
/// Unlike [`on_ci_failure_detected`], the PR's own per-branch CI is
/// green; the failure is on the **synthetic merge commit**
/// (`before_commit_sha`) that GitHub assembled when the PR was in the
/// queue. The worker must look at *that* SHA's CI logs, rebase onto
/// current `main`, and re-enqueue after pushing.
///
/// Shares the same `blocked: ci_failure` / `ci_remediation` flow as
/// per-PR CI failures but sets `failure_kind='merge_queue_rebounce'`
/// and stores `before_commit_sha` so the worker prompt and CI-log
/// fetch path know which SHA is failing.
///
/// Returns `true` when the parent transitions to `blocked: ci_failure`.
pub async fn on_merge_queue_rebounce_detected(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    head_ref_name: Option<&str>,
    _head_ref_oid: Option<&str>,
    before_commit_sha: &str,
    labels: &[String],
) -> bool {
    if auto_pr_maintenance_disabled(work_db, candidate, labels) {
        return false;
    }
    match work_db.has_active_rebase_attempt_for_pr(&candidate.pr_url) {
        Ok(true) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "ci_watch: rebase attempt active; deferring merge_queue_rebounce flip",
            );
            return false;
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "ci_watch: failed to check rebase attempt; deferring",
            );
            return false;
        }
    }
    match work_db.active_conflict_resolution_for_work_item(&candidate.work_item_id) {
        Ok(Some(_)) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "ci_watch: conflict resolution attempt active; deferring merge_queue_rebounce flip",
            );
            return false;
        }
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "ci_watch: failed to check active conflict_resolutions; deferring",
            );
            return false;
        }
    }

    // Suppression check: if the human manually moved the chore out of
    // `blocked: ci_failure` for this synthetic merge SHA, honour it.
    match work_db.is_ci_failure_suppressed(&candidate.work_item_id, before_commit_sha) {
        Ok(true) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                before_commit_sha,
                "ci_watch: ci_failure suppression active for before_commit_sha; skipping rebounce",
            );
            return false;
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "ci_watch: failed to read suppression table; continuing",
            );
        }
    }

    let used = work_db.get_ci_attempts_used(&candidate.work_item_id).unwrap_or(0);
    let budget = work_db.effective_ci_budget(&candidate.work_item_id).unwrap_or(3);
    if used >= budget {
        match work_db.mark_chore_blocked_ci_failure_exhausted(&candidate.work_item_id, &candidate.pr_url) {
            Ok(Some(_)) => {
                publisher
                    .publish_work_item_changed(
                        &candidate.product_id,
                        &candidate.work_item_id,
                        "blocked_ci_failure_exhausted",
                    )
                    .await;
                publisher
                    .publish_frontend_event_on_product(
                        &candidate.product_id,
                        FrontendEvent::CiRemediationExhausted {
                            product_id: candidate.product_id.clone(),
                            work_item_id: candidate.work_item_id.clone(),
                            pr_url: candidate.pr_url.clone(),
                            attempts_used: used,
                            budget,
                        },
                    )
                    .await;
                // No per-check names available in the rebounce path (the
                // failing SHA is the synthetic merge commit, not the PR
                // head); pass an empty slice so the body omits the list.
                emit_exhausted_attention(
                    work_db,
                    publisher,
                    &candidate.product_id,
                    &candidate.work_item_id,
                    &candidate.pr_url,
                    used,
                    budget,
                    &[],
                )
                .await;
                tracing::info!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    used,
                    budget,
                    "ci_watch: rebounce budget exhausted; parent flipped to blocked: ci_failure_exhausted",
                );
                return true;
            }
            Ok(None) => {
                tracing::debug!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    "ci_watch: rebounce ci_failure_exhausted WHERE guard missed",
                );
                return false;
            }
            Err(err) => {
                tracing::warn!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    ?err,
                    "ci_watch: failed to flip row to blocked: ci_failure_exhausted (rebounce)",
                );
                return false;
            }
        }
    }

    // Merge-queue rebounces are always `fix` — the semantic merge
    // conflict requires a worker to rebase and potentially resolve
    // incompatible changes. `retrigger` would not help.
    let pr_number = parse_pr_number(&candidate.pr_url).unwrap_or(0);

    // The `before_commit_sha` serves as `head_sha_at_trigger` so the
    // unique key `(work_item_id, head_sha_at_trigger, attempt_kind)`
    // naturally deduplicates on the synthetic merge SHA: two polls
    // that see the same bounce event hit the same key and the second
    // `INSERT OR IGNORE` is a no-op.
    let insert_result = work_db.insert_ci_remediation(CiRemediationInsertInput {
        product_id: candidate.product_id.clone(),
        work_item_id: candidate.work_item_id.clone(),
        pr_url: candidate.pr_url.clone(),
        pr_number,
        head_branch: head_ref_name.unwrap_or_default().to_owned(),
        head_sha_at_trigger: before_commit_sha.to_owned(),
        attempt_kind: "fix".to_owned(),
        consumes_budget: 1,
        failed_checks: "[]".to_owned(),
        failure_kind: "merge_queue_rebounce".to_owned(),
        before_commit_sha: Some(before_commit_sha.to_owned()),
    });
    let attempt = match insert_result {
        Ok(row) => row,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "ci_watch: failed to insert ci_remediations row (rebounce)",
            );
            None
        }
    };

    let attempt_id = attempt.as_ref().map(|a| a.id.clone());

    let task_result =
        work_db.mark_chore_blocked_ci_failure(&candidate.work_item_id, &candidate.pr_url, attempt_id.as_deref());
    let task_transitioned = match task_result {
        Ok(Some(_)) => true,
        Ok(None) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "ci_watch: rebounce WHERE guard missed; row already blocked or manually moved",
            );
            false
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "ci_watch: failed to flip row to blocked: ci_failure (rebounce)",
            );
            return false;
        }
    };

    // Phase 5 cutover (mirrors Phase 4 for on_ci_failure_detected): rebounce `fix`
    // attempts now deliver via an engine-triggered revision instead of a bespoke
    // `ci_remediation` execution. The `revision_task_id` soft-FK is the idempotency
    // latch — a repeat probe at the same before_commit_sha hits `Ok(None)` on the
    // insert above so `attempt` is `None` and this branch is skipped. The PR is
    // known-open at this point (it was in the merge queue), so a static checker
    // is correct here and avoids a redundant `gh pr view` round-trip.
    if let Some(ref a) = attempt
        && a.status == "pending"
        && a.revision_task_id.is_none()
    {
        maybe_spawn_ci_revision(
            work_db,
            publisher,
            &crate::work::StaticPrStateChecker(crate::work::PrOpenState::Open),
            candidate,
            &[], // no per-check failures for rebounce; directive uses failure_kind
            a,
        )
        .await;
    }

    if task_transitioned {
        if attempt.is_some()
            && let Err(err) = work_db.increment_ci_attempts_used(&candidate.work_item_id)
        {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                ?err,
                "ci_watch: failed to increment ci_attempts_used (rebounce)",
            );
        }
        publisher
            .publish_work_item_changed(&candidate.product_id, &candidate.work_item_id, "blocked_ci_failure")
            .await;
        if let Some(attempt) = attempt.as_ref() {
            publisher
                .publish_frontend_event_on_product(
                    &candidate.product_id,
                    FrontendEvent::CiRemediationStarted {
                        product_id: candidate.product_id.clone(),
                        work_item_id: candidate.work_item_id.clone(),
                        attempt_id: attempt.id.clone(),
                        pr_url: candidate.pr_url.clone(),
                        attempt_kind: attempt.attempt_kind.clone(),
                    },
                )
                .await;
        }
        tracing::info!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            before_commit_sha,
            head_sha_at_trigger = before_commit_sha,
            "ci_watch: merge-queue rebounce detected; parent flipped to blocked: ci_failure",
        );
        true
    } else {
        false
    }
}

/// Phase 12 #39 — soft alert when CI never starts running.
///
/// Called from `merge_poller::sweep_one` whenever the probe reports
/// `OpenPrCiStatus::InFlight` for an open PR. The engine tracks the
/// first observation per `(work_item_id, head_sha)` in
/// `ci_inflight_observations` and crosses two thresholds:
///
///   * 30 min → `warn`-level log entry.
///   * 2  h  → `warn`-level log AND a typed `CiNeverStartsAlert`
///     frontend event so the UI / activity feed surfaces it.
///
/// Each bucket is emitted at most once per pair — the row's
/// `alert_level_emitted` column monotonically advances `none → warn →
/// alert` and the WHERE guard on the update rejects regressions.
/// Returns the bucket the engine landed on this call (`"none"`,
/// `"warn"`, or `"alert"`) for tests / metrics.
pub async fn on_ci_in_flight(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
) -> &'static str {
    let Some(head_sha) = probe.head_ref_oid.as_deref() else {
        // Without a head sha we can't key the observation row.
        return "none";
    };
    let observation = match work_db.observe_ci_in_flight(&candidate.work_item_id, head_sha) {
        Ok(row) => row,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "ci_watch: failed to record InFlight observation",
            );
            return "none";
        }
    };
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let elapsed = now_secs.saturating_sub(observation.first_observed_at_secs());
    let target_bucket = if elapsed >= NEVER_STARTS_ALERT_THRESHOLD_SECS {
        "alert"
    } else if elapsed >= NEVER_STARTS_WARN_THRESHOLD_SECS {
        "warn"
    } else {
        "none"
    };
    if target_bucket == "none" || target_bucket == observation.alert_level_emitted {
        // Either we haven't crossed any threshold yet, or we already
        // emitted this bucket on a previous probe.
        return target_bucket;
    }
    // For an `alert`-bucket emit, we want to fire even if the previous
    // observation already recorded `warn` — that's the upgrade case.
    // The DB-level guard accepts `none → warn`, `none → alert`, and
    // `warn → alert` and rejects everything else.
    if let Err(err) = work_db.mark_ci_inflight_alert_level(&candidate.work_item_id, head_sha, target_bucket) {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            target_bucket,
            ?err,
            "ci_watch: failed to advance alert_level_emitted",
        );
        return match observation.alert_level_emitted.as_str() {
            "alert" => "alert",
            "warn" => "warn",
            _ => "none",
        };
    }
    let level_label = if target_bucket == "warn" { "30m" } else { "2h" };
    if target_bucket == "warn" {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            head_sha,
            elapsed,
            "ci_watch: CI has been InFlight without a definitive result for >=30m",
        );
    } else {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            head_sha,
            elapsed,
            "ci_watch: CI never-starts soft alert (>=2h InFlight on same head_sha)",
        );
        publisher
            .publish_frontend_event_on_product(
                &candidate.product_id,
                FrontendEvent::CiNeverStartsAlert {
                    product_id: candidate.product_id.clone(),
                    work_item_id: candidate.work_item_id.clone(),
                    pr_url: candidate.pr_url.clone(),
                    head_sha: head_sha.to_owned(),
                    level: level_label.to_owned(),
                    elapsed_seconds: elapsed,
                },
            )
            .await;
    }
    target_bucket
}

/// Issue #901: a newer in-progress CI run supersedes an older failing
/// result. When the probe reports `InFlight` for a PR whose chore is
/// still parked in `blocked: ci_failure` (or `ci_failure_exhausted`)
/// from a prior run, that failing result is stale — `classify_ci` yields
/// `InFlight` whenever any required check is still running, even if an
/// earlier leaf already failed (`InFlight` dominates `Fail` in the rollup
/// collapse), so the card must not keep asserting a failure while CI is
/// being re-evaluated. Flip the
/// chore back to `in_review` and emit `CiFailureCleared` so the UI drops
/// the stale "ci failing" badge. The yellow-clock indicator is written
/// separately by `update_pr_poll_state` (`ci_required_state =
/// in_progress`) in the same sweep, so once this clears the card shows a
/// single, coherent "in progress" state instead of the contradictory
/// pair the issue reported.
///
/// Guards:
///   * An *active* `ci_remediations` attempt for the **current** head SHA
///     owns the slot: its own fix push is what re-triggered CI, and its
///     in-flight chip legitimately reads "ci failing (used/budget)" —
///     i.e. "auto-fix running". We leave that case to the attempt's
///     terminal transition (`on_ci_resolved` → `CiRemediationSucceeded`,
///     or a fresh `Failing` probe), so an in-flight remediation is never
///     cleared here.
///   * If the active remediation is for an **old** head SHA (the user
///     pushed a new commit while the prior fix was still pending),
///     that remediation is stale — the new CI run is independent of it.
///     Abandon the stale row and proceed with the supersede so the badge
///     reflects the current run, not the prior one.
///   * The same `auto_pr_maintenance` opt-out as the detect / retire
///     paths is respected.
///   * Unlike `on_ci_resolved`, we do NOT reset the CI budget counter:
///     the run has not passed yet, so a subsequent failure must keep
///     consuming the remaining budget. Only a confirmed Clean transition
///     earns a fresh budget.
///
/// `current_head_sha` is the probe's `head_ref_oid` for the current
/// polling cycle. Pass `None` when the head SHA is unavailable — the
/// function then applies the conservative guard (active remediation ⇒
/// do not supersede) rather than comparing SHAs.
///
/// Returns `true` when the chore actually transitioned back to
/// `in_review` on this call; `false` (cheap no-op) when there was no
/// stale failure to supersede, a current-head remediation is active,
/// or the opt-out is set.
pub async fn on_ci_in_flight_supersedes_failure(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    labels: &[String],
    current_head_sha: Option<&str>,
) -> bool {
    if auto_pr_maintenance_disabled(work_db, candidate, labels) {
        return false;
    }

    // An active remediation attempt owns the slot — unless it is for an
    // old head SHA, in which case a new commit was pushed and the prior
    // attempt is stale. We compare `head_sha_at_trigger` (the SHA that
    // originally triggered detection) with the probe's current head SHA.
    //
    // Three cases:
    //   a) No active attempt → proceed with the supersede.
    //   b) Active attempt for the SAME head SHA → the CI-fix worker's own
    //      push re-triggered CI; the badge is legitimately "auto-fix
    //      running" — leave it to the attempt's terminal transition.
    //   c) Active attempt for a DIFFERENT (old) head SHA → a new commit
    //      landed after the attempt was created; CI is re-running at the
    //      new head independently of that attempt. Abandon the stale row
    //      and proceed with the supersede so the badge does not persist
    //      from a CI run that is no longer current.
    match work_db.active_ci_remediation_for_work_item(&candidate.work_item_id) {
        Ok(Some(active)) => {
            let stale = match current_head_sha {
                Some(current) => active.head_sha_at_trigger != current,
                None => false, // can't compare — apply conservative guard
            };
            if stale {
                // Case (c): abandon the old-head-SHA row so it no longer
                // drives the "ci failing" badge on app restart, then fall
                // through to the supersede path.
                tracing::info!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    attempt_id = %active.id,
                    stale_sha = %active.head_sha_at_trigger,
                    current_sha = ?current_head_sha,
                    "ci_watch: active remediation is for an old head SHA; \
                     abandoning stale row and superseding with current InFlight run",
                );
                if let Err(err) = work_db.mark_ci_remediation_abandoned(&active.id, "new_head_sha_inflight") {
                    tracing::warn!(
                        work_item_id = %candidate.work_item_id,
                        attempt_id = %active.id,
                        ?err,
                        "ci_watch: failed to abandon stale remediation on head-SHA change",
                    );
                }
                // Fall through — treat as no active attempt.
            } else {
                // Case (b): same head SHA → the fix is running; leave it.
                tracing::debug!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    "ci_watch: InFlight with active remediation for current head; \
                     leaving the in-flight badge to the attempt's terminal transition",
                );
                return false;
            }
        }
        Ok(None) => {} // Case (a): proceed.
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "ci_watch: failed to look up active ci_remediation; skipping InFlight supersede",
            );
            return false;
        }
    }

    let task_transitioned = match work_db.clear_chore_blocked_ci_failure(&candidate.work_item_id, &candidate.pr_url) {
        Ok(Some(_)) => true,
        // Common path: the chore is already `in_review` (no stale failure
        // to supersede). Cheap WHERE-guard no-op.
        Ok(None) => false,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "ci_watch: failed to clear stale blocked: ci_failure on InFlight",
            );
            return false;
        }
    };

    if !task_transitioned {
        // In the in_review model the parent was never blocked, but a stale
        // in-flight signal may remain from a failed revision attempt. Clear
        // it so the next Clean sweep does not re-fire the handler.
        let signal_cleared = work_db
            .clear_ci_failure_signal_only(&candidate.work_item_id)
            .unwrap_or_else(|err| {
                tracing::warn!(
                    work_item_id = %candidate.work_item_id,
                    ?err,
                    "ci_watch: failed to clear stale in-flight signal on InFlight supersede",
                );
                false
            });
        if !signal_cleared {
            return false;
        }
        // Signal was present — fall through to publish the supersede events.
    }

    publisher
        .publish_work_item_changed(
            &candidate.product_id,
            &candidate.work_item_id,
            "ci_failure_superseded_in_progress",
        )
        .await;
    publisher
        .publish_frontend_event_on_product(
            &candidate.product_id,
            FrontendEvent::CiFailureCleared {
                product_id: candidate.product_id.clone(),
                work_item_id: candidate.work_item_id.clone(),
                pr_url: candidate.pr_url.clone(),
            },
        )
        .await;
    tracing::info!(
        work_item_id = %candidate.work_item_id,
        pr_url = %candidate.pr_url,
        "ci_watch: newer in-progress CI run superseded a stale ci_failure; \
         chore returned to in_review",
    );
    true
}

/// Symmetric retire path: flip a `blocked: ci_failure` (or
/// `ci_failure_exhausted`) row back to `in_review` when the probe
/// says CI is green again. Returns `true` on transition.
///
/// Invoked on every `Clean` CI probe — the WHERE guard means an
/// already-`in_review` row is a cheap no-op. When an engine-owned
/// `ci_remediations` row covers the chore, this path also flips the
/// attempt to `succeeded` and broadcasts the typed succeeded event.
pub async fn on_ci_resolved(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    labels: &[String],
) -> bool {
    if auto_pr_maintenance_disabled(work_db, candidate, labels) {
        return false;
    }

    let attempt = match work_db.active_ci_remediation_for_work_item(&candidate.work_item_id) {
        Ok(found) => found,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "ci_watch: failed to look up active ci_remediations row; falling back to relaxed retire",
            );
            None
        }
    };

    // A merge_queue_rebounce failure must not be cleared by a clean head-branch
    // CI probe.  The PR's own CI is always green in this case — the failure is on
    // the synthetic merge commit the queue assembled, not on the PR's head ref.
    // Clearing here would immediately undo detection and create a flip-flop loop
    // where every sweep detects the rebounce and the next probe clears it.
    // The block is released only when the ci_remediation worker marks the attempt
    // succeeded (at which point `active_ci_remediation_for_work_item` returns None
    // and this guard doesn't fire).
    if attempt.as_ref().and_then(|a| a.failure_kind.as_deref()) == Some("merge_queue_rebounce") {
        tracing::debug!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            "ci_watch: skipping on_ci_resolved — active merge_queue_rebounce attempt; \
             head-branch CI clean is not the clearing signal for queue failures",
        );
        return false;
    }

    let task_result = work_db.clear_chore_blocked_ci_failure(&candidate.work_item_id, &candidate.pr_url);
    let task_transitioned = match task_result {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "ci_watch: failed to clear blocked: ci_failure",
            );
            return false;
        }
    };

    let mut attempt_transitioned = false;
    let mut parent_in_review_with_revision = false;
    if let Some(attempt) = attempt.as_ref() {
        // Parent stayed `in_review` the whole time (the shared in_review model
        // — a fix revision was in flight): the task clear above missed because
        // the status never moved to blocked, but the attempt should retire and
        // the in-flight signal must clear so `maybe_clear_blocked` does not
        // re-fire. Detect via a pending attempt that has a revision. Mirrors
        // conflict_watch::on_resolved.
        parent_in_review_with_revision =
            !task_transitioned && attempt.status == "pending" && attempt.revision_task_id.is_some();
        match work_db.mark_ci_remediation_succeeded(&attempt.id, None) {
            Ok(Some(succeeded)) => {
                attempt_transitioned = true;
                if parent_in_review_with_revision
                    && let Err(err) = work_db.clear_ci_failure_signal_only(&candidate.work_item_id)
                {
                    tracing::warn!(
                        work_item_id = %candidate.work_item_id,
                        ?err,
                        "ci_watch: failed to clear in-flight signal after retire",
                    );
                }
                publisher
                    .publish_frontend_event_on_product(
                        &candidate.product_id,
                        FrontendEvent::CiRemediationSucceeded {
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
                    "ci_watch: attempt row already terminal; skipping succeeded UPDATE",
                );
            }
            Err(err) => {
                tracing::warn!(
                    attempt_id = %attempt.id,
                    ?err,
                    "ci_watch: failed to mark ci_remediation succeeded",
                );
            }
        }
    }

    // CI has reached Clean — any leftover never-starts observation
    // (e.g. a long InFlight stretch finally produced green) is no
    // longer the relevant signal. Best-effort cleanup.
    if let Err(err) = work_db.clear_ci_inflight_observations(&candidate.work_item_id) {
        tracing::debug!(
            work_item_id = %candidate.work_item_id,
            ?err,
            "ci_watch: failed to clear inflight observations on Clean transition",
        );
    }

    if !task_transitioned && !attempt_transitioned {
        // Stale in-flight signal: in_review model, no active attempt (attempt
        // was terminal before CI went green). Clear the signal so
        // `maybe_clear_blocked` does not re-fire on every Clean sweep.
        let signal_cleared = work_db
            .clear_ci_failure_signal_only(&candidate.work_item_id)
            .unwrap_or_else(|err| {
                tracing::warn!(
                    work_item_id = %candidate.work_item_id,
                    ?err,
                    "ci_watch: failed to clear stale in-flight signal on CI resolved",
                );
                false
            });
        if !signal_cleared {
            return false;
        }
        if let Err(err) = work_db.reset_ci_attempts_used(&candidate.work_item_id) {
            tracing::debug!(
                ?err,
                "ci_watch: failed to reset ci_attempts_used after stale signal clear"
            );
        }
        publisher
            .publish_work_item_changed(&candidate.product_id, &candidate.work_item_id, "ci_failure_resolved")
            .await;
        publisher
            .publish_frontend_event_on_product(
                &candidate.product_id,
                FrontendEvent::CiFailureCleared {
                    product_id: candidate.product_id.clone(),
                    work_item_id: candidate.work_item_id.clone(),
                    pr_url: candidate.pr_url.clone(),
                },
            )
            .await;
        tracing::info!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            "ci_watch: CI back at clean; cleared stale in-flight signal (no active attempt)",
        );
        return true;
    }
    if task_transitioned {
        // Design §Q3: a successful cycle clears the counter so the
        // next failure (a new push, a new round of CI) gets a fresh
        // budget. The reset is unguarded because we only land here
        // after the parent flipped back to `in_review`; best-effort
        // because a failure here just means the next attempt starts
        // with a non-zero counter.
        if let Err(err) = work_db.reset_ci_attempts_used(&candidate.work_item_id) {
            tracing::debug!(?err, "ci_watch: failed to reset ci_attempts_used");
        }
        publisher
            .publish_work_item_changed(&candidate.product_id, &candidate.work_item_id, "ci_failure_resolved")
            .await;
        // When the task transitions back to `in_review` but no active
        // remediation attempt was found (the prior attempt was already
        // terminal — failed, abandoned, or succeeded via the rebase path),
        // emit `CiFailureCleared` so the UI can clear the `ci failing`
        // badge. The `CiRemediationSucceeded` path covers the case where
        // an active attempt is retired; this covers every other path where
        // the blocked status clears without an active attempt (T606).
        if !attempt_transitioned {
            publisher
                .publish_frontend_event_on_product(
                    &candidate.product_id,
                    FrontendEvent::CiFailureCleared {
                        product_id: candidate.product_id.clone(),
                        work_item_id: candidate.work_item_id.clone(),
                        pr_url: candidate.pr_url.clone(),
                    },
                )
                .await;
        }
    } else if parent_in_review_with_revision && attempt_transitioned {
        // In_review model: a CI-fix revision finished and CI went green.
        // The parent was never blocked so `task_transitioned` is false,
        // but the cycle is complete — reset the budget counter so the
        // next failure gets a fresh allotment.
        if let Err(err) = work_db.reset_ci_attempts_used(&candidate.work_item_id) {
            tracing::debug!(?err, "ci_watch: failed to reset ci_attempts_used after revision retire");
        }
    }
    tracing::info!(
        work_item_id = %candidate.work_item_id,
        pr_url = %candidate.pr_url,
        attempt_id = ?attempt.as_ref().map(|a| a.id.as_str()),
        task_transitioned,
        attempt_transitioned,
        "ci_watch: CI back at clean; retire path ran",
    );
    true
}

fn encode_failed_checks(failures: &[RequiredCheckFailure]) -> String {
    let records: Vec<FailedCheckRecord<'_>> = failures
        .iter()
        .map(|f| FailedCheckRecord {
            name: &f.name,
            conclusion: &f.conclusion,
            target_url: &f.target_url,
            provider: provider_str(f.provider),
            provider_job_id: f.provider_job_id.as_deref(),
        })
        .collect();
    serde_json::to_string(&records).unwrap_or_else(|_| "[]".to_owned())
}

fn provider_str(p: crate::merge_poller::CiProvider) -> &'static str {
    use crate::merge_poller::CiProvider::*;
    match p {
        Buildkite => "buildkite",
        GithubActions => "github_actions",
        Other => "other",
    }
}

/// Re-emit a fresh `ci_remediation` execution for a stranded attempt.
///
/// Called from `merge_poller::run_one_pass` for every row returned by
/// [`WorkDb::list_stranded_ci_remediation_attempts`]. A stranded row is
/// a `ci_remediations` row that is `pending` but has no live execution —
/// the canonical cause is two merge-queue dequeue events in the same sweep
/// where the first flips the task (consuming the `status='in_review'`
/// WHERE guard on `mark_chore_blocked_ci_failure`) and the second
/// inserts a ci_remediations row but cannot create an execution.
///
/// Returns `true` when an execution was successfully created.
pub async fn rescue_stranded_ci_remediation_attempt(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    attempt: &StrandedCiRemediationAttempt,
) -> bool {
    match work_db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(attempt.work_item_id.clone())
            .kind(ExecutionKind::CiRemediation)
            .status(ExecutionStatus::Ready)
            .build(),
    ) {
        Ok(_) => {
            publisher.kick_scheduler();
            tracing::info!(
                work_item_id = %attempt.work_item_id,
                attempt_id = %attempt.attempt_id,
                pr_url = %attempt.pr_url,
                "ci_watch: re-dispatched execution for stranded pending ci_remediation attempt",
            );
            true
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %attempt.work_item_id,
                attempt_id = %attempt.attempt_id,
                ?err,
                "ci_watch: failed to re-emit execution for stranded ci_remediation attempt",
            );
            false
        }
    }
}

/// Called after a PR is marked merged. Abandons any pending or running
/// `ci_remediations` rows for `work_item_id` (they are moot now that the PR
/// has shipped) and emits `CiFailureCleared` if any rows were cleaned up.
///
/// This closes the invalidation gap where a task is `blocked: ci_failure`
/// (or had an outstanding remediation row) when the PR is merged: without
/// this cleanup, the `pending` row causes `sendListCiRemediations` to
/// re-set the "ci failing" badge on every app restart, even after the
/// task is `done`.
pub async fn on_pr_merged(work_db: &WorkDb, publisher: &dyn ExecutionPublisher, candidate: &PendingMergeCheck) {
    let count = match work_db.abandon_active_ci_remediations_for_work_item(&candidate.work_item_id) {
        Ok(n) => n,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "ci_watch: failed to abandon active ci_remediations on PR merge; badge may persist",
            );
            return;
        }
    };
    if count > 0 {
        publisher
            .publish_frontend_event_on_product(
                &candidate.product_id,
                FrontendEvent::CiFailureCleared {
                    product_id: candidate.product_id.clone(),
                    work_item_id: candidate.work_item_id.clone(),
                    pr_url: candidate.pr_url.clone(),
                },
            )
            .await;
        tracing::debug!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            count,
            "ci_watch: abandoned active ci_remediations on PR merge; CiFailureCleared emitted",
        );
    }
}

#[cfg(test)]
#[path = "ci_watch_tests.rs"]
mod tests;
