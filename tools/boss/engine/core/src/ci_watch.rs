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

use boss_protocol::{CREATED_VIA_CI_FIX_PREFIX, CreateRevisionInput, FrontendEvent};
use serde::Serialize;

use crate::blocking_signal::{self, SignalKind};
use crate::coordinator::ExecutionPublisher;
use crate::merge_poller::{PrLifecycleProbe, RequiredCheckFailure, parse_pr_number, pr_labels_opt_out};
use crate::work::{
    CiRemediation, CiRemediationInsertInput, CreateExecutionInput, PendingMergeCheck,
    PrStateChecker, StrandedCiRemediationAttempt, WorkDb,
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
fn auto_pr_maintenance_disabled(
    work_db: &WorkDb,
    candidate: &PendingMergeCheck,
    labels: &[String],
) -> bool {
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
    if let Ok(Some(active)) =
        work_db.active_ci_remediation_for_work_item(&candidate.work_item_id)
    {
        if active.revision_task_id.is_some() {
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
                        .publish_work_item_changed(
                            &candidate.product_id,
                            &candidate.work_item_id,
                            "ci_revision_in_flight",
                        )
                        .await;
                }
                return reconciled;
            }
            // Parent is `in_review` (or human-moved): idempotent probe. Keep the
            // in-flight signal armed so `maybe_clear_blocked` fires on green.
            let _ = work_db.record_ci_failure_in_flight(&candidate.work_item_id, &active.id);
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
    let budget = work_db
        .effective_ci_budget(&candidate.work_item_id)
        .unwrap_or(3);
    if used >= budget {
        match work_db
            .mark_chore_blocked_ci_failure_exhausted(&candidate.work_item_id, &candidate.pr_url)
        {
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

    let task_result = work_db.mark_chore_blocked_ci_failure(
        &candidate.work_item_id,
        &candidate.pr_url,
        attempt_id.as_deref(),
    );
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
    if let Some(ref a) = attempt {
        if a.attempt_kind == "fix" && a.status == "pending" && a.revision_task_id.is_none() {
            if maybe_spawn_ci_revision(work_db, publisher, pr_checker, candidate, failures, a).await
            {
                task_unblocked_for_revision = blocking_signal::unblock_for_revision(
                    work_db,
                    SignalKind::CiFailure,
                    candidate,
                    &a.id,
                );
            }
            // If the spawn was refused (create_revision gate), the attempt is
            // abandoned and the parent stays `blocked: ci_failure` — the
            // human-attention terminal.
        }
    }

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
        if attempt.is_some() && attempt_kind == "fix" {
            if let Err(err) = work_db.increment_ci_attempts_used(&candidate.work_item_id) {
                tracing::warn!(
                    work_item_id = %candidate.work_item_id,
                    ?err,
                    "ci_watch: failed to increment ci_attempts_used",
                );
            }
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
            .publish_work_item_changed(
                &candidate.product_id,
                &candidate.work_item_id,
                change_reason,
            )
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
                match work_db.create_execution(CreateExecutionInput::builder()
                    .work_item_id(candidate.work_item_id.clone())
                    .kind("ci_remediation")
                    .status("ready")
                    .build()) {
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
                "ci_watch: create_revision failed (parent likely no longer revisable); abandoning attempt",
            );
            if let Err(abandon_err) =
                work_db.mark_ci_remediation_abandoned(&attempt.id, "revision_create_failed")
            {
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
    let budget = work_db
        .effective_ci_budget(&candidate.work_item_id)
        .unwrap_or(3);
    if used >= budget {
        match work_db
            .mark_chore_blocked_ci_failure_exhausted(&candidate.work_item_id, &candidate.pr_url)
        {
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

    let task_result = work_db.mark_chore_blocked_ci_failure(
        &candidate.work_item_id,
        &candidate.pr_url,
        attempt_id.as_deref(),
    );
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
    if let Some(ref a) = attempt {
        if a.status == "pending" && a.revision_task_id.is_none() {
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
    }

    if task_transitioned {
        if attempt.is_some() {
            if let Err(err) = work_db.increment_ci_attempts_used(&candidate.work_item_id) {
                tracing::warn!(
                    work_item_id = %candidate.work_item_id,
                    ?err,
                    "ci_watch: failed to increment ci_attempts_used (rebounce)",
                );
            }
        }
        publisher
            .publish_work_item_changed(
                &candidate.product_id,
                &candidate.work_item_id,
                "blocked_ci_failure",
            )
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
///             frontend event so the UI / activity feed surfaces it.
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
    if let Err(err) =
        work_db.mark_ci_inflight_alert_level(&candidate.work_item_id, head_sha, target_bucket)
    {
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
/// from a prior run, that failing result is stale — `classify_ci` only
/// yields `InFlight` when *no* required check is currently failing
/// (Fail dominates InFlight in the rollup collapse), so the card must
/// not keep asserting a failure while CI is being re-evaluated. Flip the
/// chore back to `in_review` and emit `CiFailureCleared` so the UI drops
/// the stale "ci failing" badge. The yellow-clock indicator is written
/// separately by `update_pr_poll_state` (`ci_required_state =
/// in_progress`) in the same sweep, so once this clears the card shows a
/// single, coherent "in progress" state instead of the contradictory
/// pair the issue reported.
///
/// Guards:
///   * An *active* `ci_remediations` attempt owns the slot: its own fix
///     push is what re-triggered CI, and its in-flight chip legitimately
///     reads "ci failing (used/budget)" — i.e. "auto-fix running". We
///     leave that case to the attempt's terminal transition
///     (`on_ci_resolved` → `CiRemediationSucceeded`, or a fresh
///     `Failing` probe), so an in-flight remediation is never cleared
///     here.
///   * The same `auto_pr_maintenance` opt-out as the detect / retire
///     paths is respected.
///   * Unlike `on_ci_resolved`, we do NOT reset the CI budget counter:
///     the run has not passed yet, so a subsequent failure must keep
///     consuming the remaining budget. Only a confirmed Clean transition
///     earns a fresh budget.
///
/// Returns `true` when the chore actually transitioned back to
/// `in_review` on this call; `false` (cheap no-op) when there was no
/// stale failure to supersede, a remediation is active, or the opt-out
/// is set.
pub async fn on_ci_in_flight_supersedes_failure(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    labels: &[String],
) -> bool {
    if auto_pr_maintenance_disabled(work_db, candidate, labels) {
        return false;
    }

    // An active remediation attempt's own re-run must not clear its
    // in-flight tracking — only a genuinely stale failure (no attempt in
    // flight) is superseded here.
    match work_db.active_ci_remediation_for_work_item(&candidate.work_item_id) {
        Ok(Some(_)) => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "ci_watch: InFlight with active remediation; leaving the in-flight badge \
                 to the attempt's terminal transition",
            );
            return false;
        }
        Ok(None) => {}
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

    let task_transitioned = match work_db
        .clear_chore_blocked_ci_failure(&candidate.work_item_id, &candidate.pr_url)
    {
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
    if attempt
        .as_ref()
        .and_then(|a| a.failure_kind.as_deref())
        == Some("merge_queue_rebounce")
    {
        tracing::debug!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            "ci_watch: skipping on_ci_resolved — active merge_queue_rebounce attempt; \
             head-branch CI clean is not the clearing signal for queue failures",
        );
        return false;
    }

    let task_result = work_db
        .clear_chore_blocked_ci_failure(&candidate.work_item_id, &candidate.pr_url);
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
                if parent_in_review_with_revision {
                    if let Err(err) =
                        work_db.clear_ci_failure_signal_only(&candidate.work_item_id)
                    {
                        tracing::warn!(
                            work_item_id = %candidate.work_item_id,
                            ?err,
                            "ci_watch: failed to clear in-flight signal after retire",
                        );
                    }
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
            tracing::debug!(?err, "ci_watch: failed to reset ci_attempts_used after stale signal clear");
        }
        publisher
            .publish_work_item_changed(
                &candidate.product_id,
                &candidate.work_item_id,
                "ci_failure_resolved",
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
            .publish_work_item_changed(
                &candidate.product_id,
                &candidate.work_item_id,
                "ci_failure_resolved",
            )
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
    match work_db.create_execution(CreateExecutionInput::builder()
        .work_item_id(attempt.work_item_id.clone())
        .kind("ci_remediation")
        .status("ready")
        .build()) {
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
pub async fn on_pr_merged(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
) {
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
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    use super::*;
    use crate::merge_poller::{CiProvider, OpenPrStatus, PrLifecycleProbe, PrLifecycleState};
    use crate::work::{CreateChoreInput, CreateProductInput, WorkDb, WorkItem, WorkItemPatch};

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

    fn candidate(product_id: &str, work_item_id: &str, pr_url: &str) -> PendingMergeCheck {
        PendingMergeCheck {
            work_item_id: work_item_id.to_owned(),
            product_id: product_id.to_owned(),
            pr_url: pr_url.to_owned(),
        }
    }

    fn probe(pr_url: &str, head_sha: &str) -> PrLifecycleProbe {
        PrLifecycleProbe {
            url: pr_url.to_owned(),
            state: PrLifecycleState::Open(OpenPrStatus::clean()),
            base_ref_oid: Some("base-1".into()),
            head_ref_oid: Some(head_sha.to_owned()),
            head_ref_name: None,
            base_ref_name: None,
            labels: Vec::new(),
            review: crate::merge_poller::PrReviewState::Unknown,
            in_merge_queue: false,
        }
    }

    fn probe_with_labels(pr_url: &str, head_sha: &str, labels: &[&str]) -> PrLifecycleProbe {
        PrLifecycleProbe {
            url: pr_url.to_owned(),
            state: PrLifecycleState::Open(OpenPrStatus::clean()),
            base_ref_oid: Some("base-1".into()),
            head_ref_oid: Some(head_sha.to_owned()),
            head_ref_name: None,
            base_ref_name: None,
            labels: labels.iter().map(|s| (*s).to_owned()).collect(),
            review: crate::merge_poller::PrReviewState::Unknown,
            in_merge_queue: false,
        }
    }

    fn one_failure() -> Vec<RequiredCheckFailure> {
        vec![RequiredCheckFailure {
            name: "ci/test".into(),
            conclusion: "FAILURE".into(),
            target_url: "https://buildkite.com/anthropic/mono/builds/42#job-uuid".into(),
            provider: CiProvider::Buildkite,
            provider_job_id: Some("job-uuid".into()),
        }]
    }

    fn chore_state(db: &WorkDb, id: &str) -> (String, Option<String>) {
        match db.get_work_item(id).unwrap() {
            WorkItem::Chore(t) => (t.status, t.blocked_reason),
            other => panic!("expected chore, got {other:?}"),
        }
    }

    /// The create-time revision gate's PR-state probe for tests. The
    /// production CI producer feeds `StaticPrStateChecker(Open)` (the poller
    /// just observed the PR open at clean mergeability); tests use the fake
    /// so `create_revision`'s `assert_parent_revisable` sees an open PR
    /// without a `gh` round-trip.
    fn fix_checker() -> crate::work::FakePrStateChecker {
        crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open)
    }

    #[tokio::test]
    async fn detection_flips_in_review_to_blocked_ci_failure() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/10";
        let (product, chore) = make_in_review(&db, "C-detect", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        let flipped = on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;
        assert!(flipped, "first detection must flip the row");

        // In the in_review model a spawned revision immediately unblocks the
        // parent back to `in_review`; `blocked: ci_failure` is transient.
        let (status, reason) = chore_state(&db, &chore);
        assert_eq!(status, "in_review");
        assert!(reason.is_none());

        let events = pub_.events.lock().await.clone();
        assert!(events.iter().any(|(_, _, r)| r == "ci_revision_in_flight"));

        let typed = pub_.typed_events.lock().await.clone();
        assert!(typed.iter().any(|(_, ev)| matches!(
            ev,
            FrontendEvent::CiRemediationStarted { .. }
        )));

        // Counter incremented by one because we created a fix-kind attempt.
        assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);
    }

    #[tokio::test]
    async fn detection_is_idempotent_on_repeated_probes() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/11";
        let (product, chore) = make_in_review(&db, "C-idem", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        let first = on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;
        let second = on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;
        assert!(first);
        assert!(!second, "second probe with same head_sha must be a no-op");

        // Counter incremented exactly once across the duplicate probes.
        assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);
    }

    #[tokio::test]
    async fn detection_defers_when_active_conflict_resolution_exists() {
        // §Q7 composed ordering: a conflict resolution attempt for
        // the same PR pre-empts the CI flow.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/12";
        let (product, chore) = make_in_review(&db, "C-defer-cr", pr);
        db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
        db.insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
            product_id: product.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.into(),
            pr_number: 12,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-1".into()),
            head_sha_before: Some("head-1".into()),
        })
        .unwrap();
        // Reset to in_review so the WHERE guard would otherwise fire.
        db.update_work_item(
            &chore,
            WorkItemPatch {
                status: Some("in_review".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        let pub_ = Arc::new(RecordingPublisher::default());
        let flipped = on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;
        assert!(!flipped, "active conflict-resolution must pre-empt CI flow");
        let (status, _) = chore_state(&db, &chore);
        assert_eq!(status, "in_review", "row stays where it was");
    }

    #[tokio::test]
    async fn detection_defers_when_active_rebase_attempt_exists() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/13";
        let (product, chore) = make_in_review(&db, "C-defer-rebase", pr);
        // Stand up the auto-rebase side table directly so the deferral
        // gate observes a non-terminal row.
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
        let flipped = on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;
        assert!(!flipped, "active rebase attempt must pre-empt CI flow");
    }

    #[tokio::test]
    async fn detection_lands_exhausted_when_budget_is_zero() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/14";
        let (product, chore) = make_in_review(&db, "C-exh", pr);
        // Set the per-product budget to 0 ("notify only").
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE products SET ci_attempt_budget = 0 WHERE id = ?1",
            [&product],
        )
        .unwrap();
        drop(conn);

        let pub_ = Arc::new(RecordingPublisher::default());
        let flipped = on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;
        assert!(flipped);
        let (status, reason) = chore_state(&db, &chore);
        assert_eq!(status, "blocked");
        assert_eq!(reason.as_deref(), Some("ci_failure_exhausted"));

        let typed = pub_.typed_events.lock().await.clone();
        assert!(typed.iter().any(|(_, ev)| matches!(
            ev,
            FrontendEvent::CiRemediationExhausted { .. }
        )));
        // No attempt row should have been inserted.
        assert!(db
            .active_ci_remediation_for_work_item(&chore)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn detection_skipped_when_pr_has_opt_out_label() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/15";
        let (product, chore) = make_in_review(&db, "C-optout", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        let flipped = on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe_with_labels(pr, "head-1", &["boss/no-auto-rebase"]),
            &one_failure(),
        )
        .await;
        assert!(!flipped);
    }

    #[tokio::test]
    async fn detection_requires_head_ref_oid() {
        // Without `headRefOid` the engine can't key the attempt row,
        // so we leave the parent alone and wait for the next probe.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/16";
        let (product, chore) = make_in_review(&db, "C-no-head", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        let mut p = probe(pr, "head-1");
        p.head_ref_oid = None;
        let flipped = on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &p,
            &one_failure(),
        )
        .await;
        assert!(!flipped);
        let (status, _) = chore_state(&db, &chore);
        assert_eq!(status, "in_review");
    }

    #[tokio::test]
    async fn full_cycle_detect_then_retire() {
        // Probe → attempt → push (simulated) → next probe Clean → retire.
        // Idempotency: a second Clean probe is a no-op.
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/17";
        let (product, chore) = make_in_review(&db, "C-cycle", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // 1. Detect.
        let detected = on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;
        assert!(detected);
        // In the in_review model the parent stays in_review while the revision runs.
        let (status, _) = chore_state(&db, &chore);
        assert_eq!(status, "in_review");

        // 2. Retire — CI is back to clean.
        let resolved = on_ci_resolved(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &[],
        )
        .await;
        assert!(resolved);
        let (status, reason) = chore_state(&db, &chore);
        assert_eq!(status, "in_review");
        assert!(reason.is_none());

        // Attempt row terminal.
        let attempts: Vec<_> = {
            let conn = rusqlite::Connection::open(dir.path().join("boss.db")).unwrap();
            let mut stmt = conn
                .prepare("SELECT status FROM ci_remediations WHERE work_item_id = ?1")
                .unwrap();
            let rows: Vec<String> = stmt
                .query_map([&chore], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            rows
        };
        assert_eq!(attempts, vec!["succeeded".to_owned()]);

        // 3. Counter reset on successful cycle.
        assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);

        // 4. Repeat retire — no-op.
        let again = on_ci_resolved(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &[],
        )
        .await;
        assert!(!again);
    }

    #[tokio::test]
    async fn retire_skipped_when_product_opt_out_flag_disabled() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/18";
        let (product, chore) = make_in_review(&db, "C-optout-retire", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // Detect first so there's something to retire.
        on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE products SET auto_pr_maintenance_enabled = 0 WHERE id = ?1",
            [&product],
        )
        .unwrap();
        drop(conn);

        let retired = on_ci_resolved(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &[],
        )
        .await;
        assert!(!retired, "opted-out product must not retire automatically");
        // In the in_review model the parent was never blocked; the retire
        // no-op leaves it in_review.
        let (status, reason) = chore_state(&db, &chore);
        assert_eq!(status, "in_review");
        assert!(reason.is_none());
    }

    /// When `on_ci_resolved` clears a `blocked: ci_failure` row but finds
    /// no active (pending/running) remediation attempt — because the prior
    /// attempt was already terminal (failed, abandoned) — it must emit
    /// `CiFailureCleared` so the UI can clear its stale `ci failing` badge
    /// without incorrectly setting the `ci auto-fixed` badge. (T606 fix)
    #[tokio::test]
    async fn retire_without_active_attempt_emits_ci_failure_cleared() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/19";
        let (product, chore) = make_in_review(&db, "C-no-active-attempt", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // 1. Detect failure → attempt created and marked failed (simulating
        //    a worker that ran but couldn't push a fix).
        on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;
        let attempt = db
            .active_ci_remediation_for_work_item(&chore)
            .unwrap()
            .expect("attempt row");
        db.mark_ci_remediation_failed(&attempt.id, "no_push_no_classification")
            .unwrap();

        // 2. CI goes green on its own — no active attempt left.
        assert!(
            db.active_ci_remediation_for_work_item(&chore)
                .unwrap()
                .is_none(),
            "attempt must be terminal before retire"
        );
        let resolved = on_ci_resolved(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &[],
        )
        .await;
        assert!(resolved, "retire must succeed even without active attempt");

        let (status, reason) = chore_state(&db, &chore);
        assert_eq!(status, "in_review");
        assert!(reason.is_none());

        // Engine must emit CiFailureCleared (not CiRemediationSucceeded)
        // so the UI clears the failure badge without setting auto-fixed.
        let typed = pub_.typed_events.lock().await.clone();
        assert!(
            typed.iter().any(|(_, ev)| matches!(
                ev,
                FrontendEvent::CiFailureCleared { pr_url, .. } if pr_url == pr
            )),
            "CiFailureCleared must be emitted when task clears without active attempt"
        );
        assert!(
            !typed.iter().any(|(_, ev)| matches!(
                ev,
                FrontendEvent::CiRemediationSucceeded { .. }
            )),
            "CiRemediationSucceeded must NOT be emitted when there is no active attempt"
        );
    }

    /// Issue #901: a chore left in `blocked: ci_failure` from a prior
    /// run is superseded once CI re-enters InFlight (no active
    /// remediation). The chore returns to `in_review`, `CiFailureCleared`
    /// is emitted so the UI drops the stale badge, and the CI budget
    /// counter is preserved (the run hasn't passed yet).
    #[tokio::test]
    async fn in_flight_supersedes_stale_ci_failure() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/901";
        let (product, chore) = make_in_review(&db, "C-supersede", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // 1. Detect failure → blocked: ci_failure, budget=1, attempt
        //    created. Then mark the attempt failed so no active
        //    remediation remains (a worker that ran but couldn't push).
        on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;
        let attempt = db
            .active_ci_remediation_for_work_item(&chore)
            .unwrap()
            .expect("attempt row");
        db.mark_ci_remediation_failed(&attempt.id, "no_push_no_classification")
            .unwrap();
        assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);

        // 2. CI re-runs (InFlight) — the stale failure is superseded.
        let cleared = on_ci_in_flight_supersedes_failure(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &[],
        )
        .await;
        assert!(cleared, "stale ci_failure must be superseded by InFlight");

        let (status, reason) = chore_state(&db, &chore);
        assert_eq!(status, "in_review");
        assert!(reason.is_none());

        let typed = pub_.typed_events.lock().await.clone();
        assert!(
            typed.iter().any(|(_, ev)| matches!(
                ev,
                FrontendEvent::CiFailureCleared { pr_url, .. } if pr_url == pr
            )),
            "CiFailureCleared must drop the stale badge",
        );
        let events = pub_.events.lock().await.clone();
        assert!(
            events
                .iter()
                .any(|(_, _, r)| r == "ci_failure_superseded_in_progress"),
        );

        // Budget is NOT reset — the re-run hasn't passed yet, so a fresh
        // failure must keep consuming the remaining allotment.
        assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);
    }

    /// An *active* remediation attempt owns the slot: its own fix push is
    /// what re-triggered CI, so its in-flight chip must not be cleared.
    /// The supersede path declines and the chore stays blocked.
    #[tokio::test]
    async fn in_flight_supersede_skips_when_active_remediation() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/902";
        let (product, chore) = make_in_review(&db, "C-active-rem", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // Detection leaves a pending (active) remediation attempt.
        on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;
        assert!(
            db.active_ci_remediation_for_work_item(&chore)
                .unwrap()
                .is_some(),
            "attempt must be active before the supersede check",
        );

        let cleared = on_ci_in_flight_supersedes_failure(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &[],
        )
        .await;
        assert!(!cleared, "active remediation must not be superseded");

        // In the in_review model the parent stays in_review while the revision runs.
        let (status, reason) = chore_state(&db, &chore);
        assert_eq!(status, "in_review");
        assert!(reason.is_none());
    }

    /// No stale failure to supersede (chore already `in_review`): the
    /// supersede path is a cheap WHERE-guard no-op and emits nothing.
    #[tokio::test]
    async fn in_flight_supersede_noop_when_in_review() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/903";
        let (product, chore) = make_in_review(&db, "C-noop", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        let cleared = on_ci_in_flight_supersedes_failure(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &[],
        )
        .await;
        assert!(!cleared, "an in_review chore has no stale failure to clear");

        let (status, _) = chore_state(&db, &chore);
        assert_eq!(status, "in_review");
        assert!(pub_.typed_events.lock().await.is_empty());
        assert!(pub_.events.lock().await.is_empty());
    }

    /// The opt-out label suppresses the supersede just like the detect /
    /// retire paths: a stale ci_failure on an opted-out PR is left alone.
    #[tokio::test]
    async fn in_flight_supersede_skipped_when_pr_has_opt_out_label() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/904";
        let (product, chore) = make_in_review(&db, "C-supersede-optout", pr);
        db.mark_chore_blocked_ci_failure(&chore, pr, None).unwrap();
        let pub_ = Arc::new(RecordingPublisher::default());

        let cleared = on_ci_in_flight_supersedes_failure(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &["boss/no-auto-rebase".to_owned()],
        )
        .await;
        assert!(!cleared, "opt-out label must suppress the supersede");

        let (status, reason) = chore_state(&db, &chore);
        assert_eq!(status, "blocked");
        assert_eq!(reason.as_deref(), Some("ci_failure"));
    }

    /// First InFlight probe records `first_observed_at` but emits
    /// nothing (no threshold crossed). A subsequent probe whose
    /// observed timestamp is rewound by >30min lands in the `warn`
    /// bucket; rewinding past 2h lands in `alert`. Repeated probes at
    /// the same bucket are no-ops (the WHERE guard rejects same-level
    /// re-emits).
    #[tokio::test]
    async fn never_starts_alert_crosses_warn_then_alert() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/30";
        let (product, chore) = make_in_review(&db, "C-never-starts", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // Probe #1: no threshold crossed.
        let level = on_ci_in_flight(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-A"),
        )
        .await;
        assert_eq!(level, "none");
        let typed_after_first = pub_.typed_events.lock().await.clone();
        assert!(typed_after_first.is_empty(), "no event before any bucket");

        // Rewind the observation timestamp by 31 min so the next probe
        // crosses the warn threshold.
        let warn_cutoff = current_unix_secs() - (31 * 60);
        rewind_inflight_observation(&db_path, &chore, "head-A", warn_cutoff);
        let level = on_ci_in_flight(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-A"),
        )
        .await;
        assert_eq!(level, "warn");
        // Still no soft-alert frontend event — warn is log-only.
        let typed_after_warn = pub_.typed_events.lock().await.clone();
        assert!(
            typed_after_warn
                .iter()
                .all(|(_, ev)| !matches!(ev, FrontendEvent::CiNeverStartsAlert { .. })),
            "warn bucket must not emit CiNeverStartsAlert event",
        );

        // A second probe at the same elapsed bucket is a no-op (the
        // alert-level WHERE guard rejects a same-level rewrite).
        let again = on_ci_in_flight(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-A"),
        )
        .await;
        assert_eq!(again, "warn");

        // Rewind past 2h so the next probe upgrades to alert.
        let alert_cutoff = current_unix_secs() - (2 * 60 * 60 + 60);
        rewind_inflight_observation(&db_path, &chore, "head-A", alert_cutoff);
        let level = on_ci_in_flight(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-A"),
        )
        .await;
        assert_eq!(level, "alert");
        let typed = pub_.typed_events.lock().await.clone();
        assert!(
            typed.iter().any(|(_, ev)| matches!(
                ev,
                FrontendEvent::CiNeverStartsAlert {
                    level,
                    ..
                } if level == "2h"
            )),
            "alert bucket must emit CiNeverStartsAlert with level=2h",
        );
    }

    /// A fresh push (new head sha) keys observations on its own row,
    /// so the timer restarts from zero and the previous bucket doesn't
    /// carry over.
    #[tokio::test]
    async fn never_starts_alert_resets_on_new_head_sha() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/31";
        let (product, chore) = make_in_review(&db, "C-new-head", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // Drive head-A all the way to `alert`.
        on_ci_in_flight(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-A"),
        )
        .await;
        rewind_inflight_observation(
            &db_path,
            &chore,
            "head-A",
            current_unix_secs() - (3 * 60 * 60),
        );
        let level = on_ci_in_flight(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-A"),
        )
        .await;
        assert_eq!(level, "alert");

        // A new head sha starts fresh.
        let level = on_ci_in_flight(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-B"),
        )
        .await;
        assert_eq!(level, "none", "new head sha must reset the timer");
    }

    /// When the engine flips the chore to `blocked: ci_failure` (CI
    /// transitions from InFlight to Failing), the leftover observation
    /// row must be cleared so a later InFlight stretch starts fresh.
    #[tokio::test]
    async fn detection_clears_inflight_observation() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/32";
        let (product, chore) = make_in_review(&db, "C-clear-on-detect", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        on_ci_in_flight(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
        )
        .await;
        let n: i64 = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM ci_inflight_observations WHERE work_item_id = ?1",
                [&chore],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "observation row exists after InFlight probe");

        on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;
        let n: i64 = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM ci_inflight_observations WHERE work_item_id = ?1",
                [&chore],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "Failing detection must clear inflight observations");
    }

    fn current_unix_secs() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// Rewrite the `first_observed_at` timestamp on a
    /// `ci_inflight_observations` row to simulate the passage of time
    /// without sleeping. Used by the never-starts-alert tests.
    fn rewind_inflight_observation(
        db_path: &std::path::Path,
        work_item_id: &str,
        head_sha: &str,
        when_unix_secs: i64,
    ) {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "UPDATE ci_inflight_observations
                SET first_observed_at = ?3
              WHERE work_item_id = ?1 AND head_sha = ?2",
            rusqlite::params![work_item_id, head_sha, when_unix_secs.to_string()],
        )
        .unwrap();
    }

    #[test]
    fn encode_failed_checks_round_trip() {
        let json = super::encode_failed_checks(&[RequiredCheckFailure {
            name: "ci/test".into(),
            conclusion: "FAILURE".into(),
            target_url:
                "https://github.com/foo/bar/actions/runs/1/job/2".into(),
            provider: CiProvider::GithubActions,
            provider_job_id: Some("2".into()),
        }]);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let item = &arr[0];
        assert_eq!(item["name"], "ci/test");
        assert_eq!(item["provider"], "github_actions");
        assert_eq!(item["provider_job_id"], "2");
    }

    // ----- Phase 9 #28: pre-triage classification permutations ----------

    fn failure(name: &str, conclusion: &str) -> RequiredCheckFailure {
        RequiredCheckFailure {
            name: name.into(),
            conclusion: conclusion.into(),
            target_url: "https://buildkite.com/foo/bar/builds/1#x".into(),
            provider: CiProvider::Buildkite,
            provider_job_id: Some("x".into()),
        }
    }

    #[test]
    fn pre_triage_all_startup_failure_routes_to_retrigger() {
        let fs = [failure("a", "STARTUP_FAILURE"), failure("b", "STARTUP_FAILURE")];
        assert_eq!(super::classify_pre_triage(&fs), "retrigger");
    }

    #[test]
    fn pre_triage_mixed_startup_and_cancelled_routes_to_retrigger() {
        let fs = [failure("a", "STARTUP_FAILURE"), failure("b", "CANCELLED")];
        assert_eq!(super::classify_pre_triage(&fs), "retrigger");
    }

    #[test]
    fn pre_triage_one_real_failure_routes_to_fix() {
        let fs = [failure("a", "STARTUP_FAILURE"), failure("b", "FAILURE")];
        assert_eq!(super::classify_pre_triage(&fs), "fix");
    }

    #[test]
    fn pre_triage_all_failure_routes_to_fix() {
        let fs = [failure("a", "FAILURE"), failure("b", "TIMED_OUT")];
        assert_eq!(super::classify_pre_triage(&fs), "fix");
    }

    #[test]
    fn pre_triage_action_required_routes_to_fix() {
        // ACTION_REQUIRED isn't unambiguous infra — it needs a human or
        // a worker triage decision, so it stays on the fix path.
        let fs = [failure("a", "ACTION_REQUIRED")];
        assert_eq!(super::classify_pre_triage(&fs), "fix");
    }

    #[test]
    fn pre_triage_empty_defaults_to_fix() {
        assert_eq!(super::classify_pre_triage(&[]), "fix");
    }

    // ----- Phase 4 cutover: engine-triggered revision as the fix vehicle -----

    #[tokio::test]
    async fn detection_spawns_revision_and_stamps_attempt() {
        // A genuinely-new `fix`-kind CI failure creates a `kind=revision`
        // task (parent = chore, ci-fix provenance), stamps the ledger row's
        // `revision_task_id`, and creates NO bespoke ci_remediation
        // execution — the dormant path stays dormant and the row is hidden
        // from the rescue recovery query.
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/100";
        let (product, chore) = make_in_review(&db, "C-rev-spawn", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        let flipped = on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;
        assert!(flipped);

        let attempt = db
            .active_ci_remediation_for_work_item(&chore)
            .unwrap()
            .expect("a pending attempt row must exist");
        assert_eq!(attempt.status, "pending");
        assert_eq!(attempt.attempt_kind, "fix");
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
        assert_eq!(revision.created_via, format!("ci-fix:{}", attempt.id));
        assert_eq!(revision.description, "Fix failing CI: ci/test");

        // No bespoke ci_remediation execution: the revision rides the
        // reconcile loop's revision_implementation dispatch instead.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM work_executions WHERE work_item_id = ?1 AND kind = ?2",
                rusqlite::params![&chore, "ci_remediation"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "cutover must not create a ci_remediation execution");

        // The revision-backed row is invisible to the dormant rescue path.
        assert!(
            db.list_stranded_ci_remediation_attempts().unwrap().is_empty(),
            "revision-backed attempt must be excluded from the rescue query",
        );
    }

    #[tokio::test]
    async fn detection_idempotent_does_not_double_spawn_revision() {
        // Re-firing on the same head sha reuses the existing attempt (whose
        // revision_task_id is already set) and spawns no second revision.
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/101";
        let (product, chore) = make_in_review(&db, "C-rev-idem", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;
        // Reset to in_review so the second probe re-enters the primary flip
        // path with the same head sha (UNIQUE collision on the ledger).
        db.update_work_item(
            &chore,
            WorkItemPatch {
                status: Some("in_review".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;

        let attempts = db
            .list_ci_remediations(None, &[], Some(&chore), None)
            .unwrap();
        assert_eq!(attempts.len(), 1, "same head sha must not stack attempts");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let revisions: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
                rusqlite::params![&chore],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(revisions, 1, "same head sha must not stack revisions");
    }

    #[tokio::test]
    async fn retrigger_creates_bespoke_execution_and_no_revision() {
        // `retrigger` produces no commit, so it stays on the bespoke
        // ci_remediation execution kind (design Q6) and never spawns a
        // revision or consumes budget.
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/102";
        let (product, chore) = make_in_review(&db, "C-retrigger", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // All-infra failures classify as `retrigger`.
        let infra = vec![failure("ci/flaky", "STARTUP_FAILURE")];
        let flipped = on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &infra,
        )
        .await;
        assert!(flipped);

        let attempt = db
            .active_ci_remediation_for_work_item(&chore)
            .unwrap()
            .expect("a pending attempt row must exist");
        assert_eq!(attempt.attempt_kind, "retrigger");
        assert!(
            attempt.revision_task_id.is_none(),
            "retrigger must not spawn a revision",
        );

        // Exactly one bespoke ci_remediation execution; no revision task.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let exec_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM work_executions WHERE work_item_id = ?1 AND kind = 'ci_remediation'",
                rusqlite::params![&chore],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exec_count, 1, "retrigger must park a ci_remediation execution");
        let revisions: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE kind = 'revision'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(revisions, 0, "retrigger must not create a revision");

        // Retrigger does not consume the fix budget.
        assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);
    }

    // ----- Reconciled 2026-05-17 layered design call: rebase-first success ----

    #[tokio::test]
    async fn rebase_only_success_refunds_budget_slot() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/200";
        let (product, chore) = make_in_review(&db, "C-rebase", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // 1. Detect a fix-kind failure — counter bumps to 1.
        let flipped = on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;
        assert!(flipped);
        assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);

        // 2. Worker rebases onto base HEAD and reports green CI without
        //    a code change: rebase-only success path.
        let attempt = db
            .active_ci_remediation_for_work_item(&chore)
            .unwrap()
            .expect("attempt row");
        let updated = db
            .mark_ci_remediation_succeeded_via_rebase(&attempt.id)
            .unwrap()
            .expect("WHERE guard hit");

        assert_eq!(updated.status, "succeeded");
        assert_eq!(updated.consumes_budget, 0);
        assert_eq!(updated.failure_reason.as_deref(), Some("rebase_only"));

        // 3. Counter refunded: budget slot is NOT consumed.
        assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);

        // 4. Idempotent — repeat is a no-op.
        let again = db
            .mark_ci_remediation_succeeded_via_rebase(&attempt.id)
            .unwrap();
        assert!(again.is_none(), "second call must be a no-op");
        assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);
    }

    // ----- Merge-queue rebounce detection (T605 regression, PR #690) -----

    /// A PR whose head-branch CI is all green but that was removed from
    /// the merge queue with `reason=FAILED_CHECKS` must flip its owning
    /// chore to `blocked: ci_failure` and park a `ci_remediation` execution.
    ///
    /// This is the basic reproducer for the T604 missed-detection: the
    /// engine must act on the `RemovedFromMergeQueueEvent` timeline signal,
    /// not on the per-PR `statusCheckRollup` (which stays SUCCESS after a
    /// dequeue).
    #[tokio::test]
    async fn rebounce_flips_in_review_to_blocked_ci_failure() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/500";
        let (product, chore) = make_in_review(&db, "C-rebounce-detect", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        let flipped = on_merge_queue_rebounce_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            Some("feature-branch"),
            None,
            "synthetic-merge-sha-abc",
            &[],
        )
        .await;
        assert!(flipped, "rebounce detection must flip chore to ci_failure");

        let (status, reason) = chore_state(&db, &chore);
        assert_eq!(status, "blocked");
        assert_eq!(reason.as_deref(), Some("ci_failure"));

        // Phase 5 cutover: no bespoke ci_remediation execution — the fix
        // delivers via an engine-triggered revision instead.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let exec_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM work_executions
                  WHERE work_item_id = ?1 AND kind = 'ci_remediation'",
                rusqlite::params![&chore],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exec_count, 0, "cutover: no bespoke ci_remediation execution");
        let rev_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
                rusqlite::params![&chore],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rev_count, 1, "rebounce must spawn exactly one revision task");

        // The ci_remediations row must record the failure as a queue rebounce
        // and have its revision_task_id stamped.
        let attempt = db
            .active_ci_remediation_for_work_item(&chore)
            .unwrap()
            .expect("active attempt row");
        assert_eq!(
            attempt.failure_kind.as_deref(),
            Some("merge_queue_rebounce")
        );
        assert_eq!(
            attempt.before_commit_sha.as_deref(),
            Some("synthetic-merge-sha-abc")
        );
        assert!(
            attempt.revision_task_id.is_some(),
            "attempt must have revision_task_id stamped"
        );
    }

    /// THE REGRESSION (T604 / PR #690 04:44Z miss): a clean head-branch CI
    /// probe must NOT clear a `merge_queue_rebounce` block.
    ///
    /// Before the fix, `on_ci_resolved` treated "head-branch CI is green" as
    /// a sufficient clearing signal for ALL ci_failure reasons.  For a
    /// rebounce, the PR's own CI is *always* green (the failure is on the
    /// synthetic merge commit), so every sweep immediately un-blocked the
    /// chore, preventing detection from sticking.
    #[tokio::test]
    async fn rebounce_block_not_cleared_by_clean_head_branch_ci() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/501";
        let (product, chore) = make_in_review(&db, "C-rebounce-noclr", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // Step 1: detect the rebounce — chore flips to blocked: ci_failure.
        let flipped = on_merge_queue_rebounce_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            Some("feature-branch"),
            None,
            "synthetic-sha-xyz",
            &[],
        )
        .await;
        assert!(flipped);

        // Step 2: simulate the merge_poller's next sweep — the head-branch CI
        // probe returns Clean (statusCheckRollup is all SUCCESS), so sweep_one
        // calls on_ci_resolved.  This must NOT clear the rebounce block.
        let cleared = on_ci_resolved(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &[],
        )
        .await;
        assert!(
            !cleared,
            "on_ci_resolved must not clear a merge_queue_rebounce block based on \
             head-branch CI; the PR's own CI is always green in this case"
        );

        // Chore must still be blocked after the clean probe.
        let (status, reason) = chore_state(&db, &chore);
        assert_eq!(status, "blocked");
        assert_eq!(reason.as_deref(), Some("ci_failure"));
    }

    /// A second probe of the same dequeue event (same `before_commit_sha`)
    /// is idempotent: the INSERT OR IGNORE is a no-op, but the chore stays
    /// blocked and no new execution is created.
    #[tokio::test]
    async fn rebounce_detection_idempotent_on_same_sha() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/502";
        let (product, chore) = make_in_review(&db, "C-rebounce-idem", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        let first = on_merge_queue_rebounce_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            Some("feature"),
            None,
            "sha-A",
            &[],
        )
        .await;
        // Repeat for the same SHA (as would happen when the same dequeue event
        // appears in the timeline across consecutive sweeps).
        let second = on_merge_queue_rebounce_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            Some("feature"),
            None,
            "sha-A",
            &[],
        )
        .await;
        assert!(first, "first detection must flip the chore");
        assert!(!second, "second probe for same SHA must be a no-op");

        let (status, reason) = chore_state(&db, &chore);
        assert_eq!(status, "blocked");
        assert_eq!(reason.as_deref(), Some("ci_failure"));

        // Phase 5 cutover: exactly one revision, no ci_remediation executions.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let exec_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM work_executions
                  WHERE work_item_id = ?1 AND kind = 'ci_remediation'",
                rusqlite::params![&chore],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exec_count, 0, "cutover: no bespoke ci_remediation execution");
        let rev_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
                rusqlite::params![&chore],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rev_count, 1, "exactly one revision; duplicate probe must not spawn a second");
    }

    /// After the worker marks the attempt succeeded, the next `on_ci_resolved`
    /// call (with clean head-branch CI) should clear the rebounce block — that
    /// is the correct terminal path.
    #[tokio::test]
    async fn rebounce_block_clears_after_worker_succeeds() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/503";
        let (product, chore) = make_in_review(&db, "C-rebounce-retire", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // 1. Detect.
        on_merge_queue_rebounce_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            Some("feature"),
            None,
            "sha-Q",
            &[],
        )
        .await;

        let attempt = db
            .active_ci_remediation_for_work_item(&chore)
            .unwrap()
            .expect("attempt row");

        // 2. Worker marks attempt succeeded (re-enqueued the PR).
        db.mark_ci_remediation_succeeded(&attempt.id, None)
            .unwrap()
            .expect("succeeded update");

        // 3. Now on_ci_resolved fires (head-branch CI still clean) — no active
        //    attempt exists, so the rebounce guard does not fire and the block
        //    is cleared correctly.
        let cleared = on_ci_resolved(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &[],
        )
        .await;
        assert!(cleared, "after worker succeeds, on_ci_resolved must clear the block");

        let (status, _) = chore_state(&db, &chore);
        assert_eq!(status, "in_review");
    }

    // ----- Back-to-back dequeue regression (T628 / PR #718 06:51Z miss) -----

    /// Reproducer for T628: a PR that was dequeued, manually re-queued,
    /// and dequeued again must end up with a parked `ci_remediation`
    /// execution for the second dequeue's SHA — without requiring the
    /// first dequeue's worker to have completed.
    ///
    /// Sequence:
    ///   1. Chore in_review; first dequeue (SHA_1) detected → blocked, EXEC-1 created.
    ///   2. Worker marks SHA_1 succeeded_via_rebase (human re-queued the PR).
    ///   3. on_ci_resolved clears the block → chore back to in_review.
    ///   4. Next sweep sees both SHA_1 and SHA_2 in the timeline:
    ///      - SHA_1: INSERT IGNORED (key exists, row terminal) → attempt=None;
    ///               mark_chore_blocked_ci_failure succeeds (chore in_review) →
    ///               chore blocked, but NO execution (attempt is None).
    ///      - SHA_2: INSERT succeeds → attempt=Some; mark_chore_blocked_ci_failure
    ///               WHERE-guard misses (chore already blocked) → no execution.
    ///   5. SHA_2's attempt gets a revision immediately via `maybe_spawn_ci_revision`
    ///      (called even when task_transitioned=false), so it is never stranded.
    ///
    /// Detection must not require a live worker on the chore.
    #[tokio::test]
    async fn back_to_back_rebounce_parks_execution_for_second_dequeue() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("boss.db");
        let db = WorkDb::open(db_path.clone()).unwrap();
        let pr = "https://github.com/foo/bar/pull/718";
        let (product, chore) = make_in_review(&db, "C-t628-backtoback", pr);
        let pub_ = Arc::new(RecordingPublisher::default());

        // Step 1: first dequeue (SHA_1) → chore flips to blocked, revision spawned.
        let first = on_merge_queue_rebounce_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            Some("feature"),
            None,
            "sha-merge-1",
            &[],
        )
        .await;
        assert!(first, "first rebounce must flip chore to ci_failure");
        let (status, reason) = chore_state(&db, &chore);
        assert_eq!(status, "blocked");
        assert_eq!(reason.as_deref(), Some("ci_failure"));
        {
            // Phase 5 cutover: no bespoke ci_remediation execution; a revision is
            // spawned instead.
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM work_executions
                      WHERE work_item_id = ?1 AND kind = 'ci_remediation'",
                    rusqlite::params![&chore],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 0, "cutover: no ci_remediation execution after first dequeue");
            let r: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
                    rusqlite::params![&chore],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(r, 1, "exactly one revision after first dequeue");
        }

        // Step 2: mark SHA_1's ci_remediations row succeeded_via_rebase (PR re-queued
        // by human). In production a revision_implementation worker does the push and
        // the poller retires the ledger row; here we use the DB helper directly.
        let sha1_attempt = db
            .active_ci_remediation_for_work_item(&chore)
            .unwrap()
            .expect("sha1 attempt row");
        db.mark_ci_remediation_succeeded_via_rebase(&sha1_attempt.id)
            .unwrap()
            .expect("succeeded_via_rebase update");

        // Step 3: on_ci_resolved clears the block → chore in_review again.
        let cleared = on_ci_resolved(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            &[],
        )
        .await;
        assert!(cleared, "on_ci_resolved must clear the block after SHA_1 is terminal");
        let (status, _) = chore_state(&db, &chore);
        assert_eq!(status, "in_review");

        // Step 4a: next sweep replays SHA_1 — INSERT is ignored (key exists, row
        // terminal). attempt=None → task flips (WHERE guard matches) but NO new
        // revision (no attempt to stamp).
        let sha1_replay = on_merge_queue_rebounce_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            Some("feature"),
            None,
            "sha-merge-1",
            &[],
        )
        .await;
        assert!(sha1_replay, "sha1 replay must flip chore (INSERT ignored, task_transitioned=true)");
        let (status, reason) = chore_state(&db, &chore);
        assert_eq!(status, "blocked");
        assert_eq!(reason.as_deref(), Some("ci_failure"));
        // Still just the original revision from step 1.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let r: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
                    rusqlite::params![&chore],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(r, 1, "sha1 replay must not spawn a second revision");
        }

        // Step 4b: same sweep also sees SHA_2 — INSERT succeeds (new key), but
        // mark_chore_blocked_ci_failure WHERE-guard misses (chore already blocked).
        // Phase 5 fix: maybe_spawn_ci_revision is called regardless of
        // task_transitioned, so SHA_2 gets its own revision immediately.
        let sha2_detect = on_merge_queue_rebounce_detected(
            &db,
            pub_.as_ref(),
            &candidate(&product, &chore, pr),
            Some("feature"),
            None,
            "sha-merge-2",
            &[],
        )
        .await;
        assert!(
            !sha2_detect,
            "sha2 detection must return false — task already blocked, WHERE guard missed"
        );
        // SHA_2's ci_remediations row must exist as pending with revision_task_id stamped.
        let sha2_attempt = {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let pending: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM ci_remediations
                      WHERE work_item_id = ?1 AND head_sha_at_trigger = 'sha-merge-2'
                        AND status = 'pending'",
                    rusqlite::params![&chore],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(pending, 1, "sha2 ci_remediations row must be pending");
            db.active_ci_remediation_for_work_item(&chore)
                .unwrap()
                .expect("sha2 attempt row")
        };
        assert!(
            sha2_attempt.revision_task_id.is_some(),
            "sha2 attempt must have a revision immediately — no stranding"
        );
        // Two revisions total: one for SHA_1, one for SHA_2.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let r: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
                    rusqlite::params![&chore],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(r, 2, "sha2 must have its own revision; total revisions must be 2");
        }
        let (status, reason) = chore_state(&db, &chore);
        assert_eq!(status, "blocked");
        assert_eq!(reason.as_deref(), Some("ci_failure"));

        // Sanity: no stranded ci_remediation attempts — sha2 has revision_task_id.
        let stranded = db.list_stranded_ci_remediation_attempts().unwrap();
        assert!(
            stranded.is_empty(),
            "no stranded attempts: sha2 has revision_task_id so it is excluded from rescue"
        );
    }
}
