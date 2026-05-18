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

use boss_protocol::FrontendEvent;
use serde::Serialize;

use crate::coordinator::ExecutionPublisher;
use crate::merge_poller::{PrLifecycleProbe, RequiredCheckFailure, parse_pr_number, pr_labels_opt_out};
use crate::work::{CiRemediationInsertInput, PendingMergeCheck, WorkDb};

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
pub async fn on_ci_failure_detected(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
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
    let all_infra = failures
        .iter()
        .all(|f| matches!(f.conclusion.as_str(), "STARTUP_FAILURE" | "CANCELLED"));
    let attempt_kind = if all_infra { "retrigger" } else { "fix" };
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
        head_branch: String::new(),
        head_sha_at_trigger: head_sha.to_owned(),
        attempt_kind: attempt_kind.to_owned(),
        consumes_budget,
        failed_checks: failed_checks_json,
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

    if task_transitioned {
        // Bump the budget counter only when the row actually
        // transitioned AND we created a fix-kind attempt — the design
        // (§Q3) says the counter increments when "a fix attempt
        // actually progresses past the worker's go/no-go." For Phase 8
        // we approximate that with "the engine successfully created a
        // fix-kind attempt"; Phase 9 will refine to wait for the
        // worker's classify call.
        if attempt.is_some() && attempt_kind == "fix" {
            if let Err(err) = work_db.increment_ci_attempts_used(&candidate.work_item_id) {
                tracing::warn!(
                    work_item_id = %candidate.work_item_id,
                    ?err,
                    "ci_watch: failed to increment ci_attempts_used",
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
            head_sha,
            attempt_kind,
            failures = failures.len(),
            "ci_watch: CI failure detected; parent flipped to blocked: ci_failure",
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
    if let Some(attempt) = attempt.as_ref() {
        match work_db.mark_ci_remediation_succeeded(&attempt.id, None) {
            Ok(Some(succeeded)) => {
                attempt_transitioned = true;
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
        return false;
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
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;
        assert!(flipped, "first detection must flip the row");

        let (status, reason) = chore_state(&db, &chore);
        assert_eq!(status, "blocked");
        assert_eq!(reason.as_deref(), Some("ci_failure"));

        let events = pub_.events.lock().await.clone();
        assert!(events.iter().any(|(_, _, r)| r == "blocked_ci_failure"));

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
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;
        let second = on_ci_failure_detected(
            &db,
            pub_.as_ref(),
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
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await;
        assert!(detected);
        let (status, _) = chore_state(&db, &chore);
        assert_eq!(status, "blocked");

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
}
