//! The `automation_scheduler` interval loop (Maint task 5).
//!
//! Each tick, for every enabled `schedule`-triggered automation that is due,
//! this loop decides what to do with the occurrence and records the decision
//! in `automation_runs`. The actual triage *execution* (creating the
//! `automation_triage` work_execution and rendering the preamble) is Maint
//! task 6 and is reached only through the [`TriageDispatcher`] seam — this
//! task ships the decision engine, the occurrence math (see
//! [`crate::automation_schedule`]), and the run-history writes.
//!
//! ## Per-tick decision, in order
//!
//! 1. **Initialise** — `next_due_at IS NULL` (never scheduled): compute the
//!    next occurrence and park it; do not fire this tick.
//! 2. **Not due** — `now < next_due_at`: nothing to do.
//! 3. **Catch-up collapse** — walk forward past every occurrence `<= now`,
//!    so a backlog accumulated while the laptop was asleep collapses to the
//!    single most-recent occurrence instead of firing a stampede.
//! 4. **Skip-if-stale** — if that most-recent occurrence is older than the
//!    catch-up window, it is stale: record a `skipped` run (unless we
//!    already attempted it) and advance to the next occurrence.
//! 5. **Open-task gate** — if the automation is already at its
//!    `open_task_limit`, record `suppressed_at_limit` and advance (so a
//!    capped automation doesn't fire a backlog the instant a task merges).
//! 6. **Fire** — dispatch triage. On success the occurrence is recorded as
//!    `failed_will_retry` (the pessimistic default; the task-6 detector
//!    flips it once the worker reaches a decision) and the schedule
//!    advances. On a transient pre-start failure the occurrence is *held*
//!    (`next_due_at` unchanged) for retry.
//!
//! ## A deliberate refinement of the design's skip rule
//!
//! The design (`maintenance-tasks.md` §"Scheduling semantics" step 3)
//! phrases skip-if-imminent as `following - now <= catch_up_window`. Taken
//! literally that is degenerate for sub-window cron periods: an
//! every-5-minute job would have `following - now ≈ 5min <= 15min` on *every*
//! tick and would skip every fire. We implement the equivalent-intent rule
//! `staleness = now - most_recent_occurrence > catch_up_window`, which
//! reproduces all of the design's worked examples (a daily 2pm job missed
//! until 1:50pm next day correctly skips to the real 2pm; a 10-minute-late
//! wake catches up) and is correct across all cron frequencies.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use boss_protocol::{
    AUTOMATION_OUTCOME_FAILED_WILL_RETRY, AUTOMATION_OUTCOME_SKIPPED,
    AUTOMATION_OUTCOME_SUPPRESSED_AT_LIMIT, Automation, AutomationTrigger,
};

use crate::automation_schedule::{next_occurrence_after, parse_cron, parse_timezone};
use crate::work::{AutomationFireRecord, WorkDb};

/// Default scheduler tick interval. Matches the design's
/// `spawn_loop(... Duration::from_secs(30) ...)`.
pub const AUTOMATION_SCHEDULER_INTERVAL: Duration = Duration::from_secs(30);

/// Default catch-up window: an occurrence missed by more than this is
/// considered stale and skipped. 15 minutes per the design — long enough
/// that a brief sleep/wake doesn't lose a daily job, short enough that a
/// "2pm weekday" job missed until ~2pm next day skips to the real 2pm.
/// Overridable per automation via `automations.catch_up_window_secs`.
pub const DEFAULT_CATCH_UP_WINDOW_SECS: i64 = 15 * 60;

/// Upper bound on how many occurrences the catch-up collapse will walk in a
/// single tick. Protects against a pathological high-frequency cron after a
/// very long outage (e.g. an every-minute job offline for weeks); such a
/// case converges over a few ticks instead of doing unbounded work in one.
const MAX_CATCH_UP_COLLAPSE: u32 = 10_000;

/// Result of attempting to dispatch a triage execution for a fired
/// occurrence. The actual execution machinery is Maint task 6; this enum is
/// the seam the scheduler decides `advance` vs `hold` on.
#[derive(Debug, Clone)]
pub enum TriageDispatch {
    /// A triage `work_execution` was created and enqueued. The occurrence is
    /// recorded `failed_will_retry` (pessimistic) with this execution id and
    /// the schedule advances; the task-6 detector finalises the outcome.
    Dispatched { execution_id: String },
    /// A transient pre-start failure (cube lease error, git remote
    /// unreachable, product repo unresolvable). The occurrence is held for
    /// retry — `next_due_at` is not advanced.
    TransientFailure { detail: String },
}

/// The fire seam. The scheduler calls this when an occurrence is due, under
/// cap, and not stale. Implemented for real in Maint task 6; task 5 wires
/// [`LoggingTriageDispatcher`].
#[async_trait]
pub trait TriageDispatcher: Send + Sync {
    async fn dispatch_triage(
        &self,
        automation: &Automation,
        scheduled_for_epoch: i64,
    ) -> TriageDispatch;
}

/// Task-5 placeholder dispatcher: the triage execution kind, preamble, and
/// outcome detector are Maint task 6. Until that lands, every fire reports a
/// transient failure so the occurrence is *held* (recorded
/// `failed_will_retry`, schedule not advanced) rather than silently
/// dropped — the same state a real VPN-down pre-start failure produces.
/// With zero automations configured the loop is inert; the first time a real
/// automation comes due this logs a single warning naming the missing piece.
#[derive(Debug, Default)]
pub struct LoggingTriageDispatcher;

#[async_trait]
impl TriageDispatcher for LoggingTriageDispatcher {
    async fn dispatch_triage(
        &self,
        automation: &Automation,
        scheduled_for_epoch: i64,
    ) -> TriageDispatch {
        tracing::warn!(
            automation_id = %automation.id,
            scheduled_for = scheduled_for_epoch,
            "automation due to fire, but triage dispatch is not yet implemented \
             (Maint task 6); holding occurrence as failed_will_retry",
        );
        TriageDispatch::TransientFailure {
            detail: "triage dispatch not yet implemented (Maint task 6)".to_owned(),
        }
    }
}

/// Per-pass counters, for logging and tests. Constructed via `default()`
/// and incremented in place; the `bon::Builder` derive is present only to
/// satisfy the repo's giant-struct convention (`checkleft`'s
/// rust-giant-structs-use-builder, which flags 6+ named fields) — the
/// scheduler never builds one.
#[derive(Debug, Default, PartialEq, Eq)]
#[derive(bon::Builder)]
pub struct AutomationSchedulerPass {
    /// Due automations evaluated this pass.
    pub evaluated: usize,
    /// Automations whose `next_due_at` was initialised this pass (no fire).
    pub initialized: usize,
    /// Occurrences fired (triage dispatched).
    pub fired: usize,
    /// Occurrences suppressed at the open-task limit.
    pub suppressed: usize,
    /// Stale occurrences skipped.
    pub skipped_stale: usize,
    /// Fires held after a transient dispatch failure.
    pub held_transient: usize,
    /// Automations skipped this pass due to a malformed cron/timezone.
    pub config_errors: usize,
}

/// Spawn the scheduler loop. Fires immediately on boot (so a daily job whose
/// occurrence elapsed while the engine was down is caught up without waiting
/// a full interval) then ticks every `interval`.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    dispatcher: Arc<dyn TriageDispatcher>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let now = now_epoch();
            let pass = run_one_pass(work_db.as_ref(), now, dispatcher.as_ref()).await;
            if pass.evaluated > 0 {
                tracing::info!(
                    evaluated = pass.evaluated,
                    initialized = pass.initialized,
                    fired = pass.fired,
                    suppressed = pass.suppressed,
                    skipped_stale = pass.skipped_stale,
                    held_transient = pass.held_transient,
                    config_errors = pass.config_errors,
                    "automation scheduler: pass complete",
                );
            }
            tokio::time::sleep(interval).await;
        }
    })
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Run a single scheduler pass against `now_epoch` (UTC seconds). Pure of
/// wall-clock reads so DST and catch-up behaviour is deterministically
/// testable.
pub async fn run_one_pass(
    work_db: &WorkDb,
    now_epoch: i64,
    dispatcher: &dyn TriageDispatcher,
) -> AutomationSchedulerPass {
    let mut pass = AutomationSchedulerPass::default();

    let due = match work_db.list_due_automations(now_epoch) {
        Ok(due) => due,
        Err(err) => {
            tracing::warn!(?err, "automation scheduler: failed to list due automations; skipping pass");
            return pass;
        }
    };

    for automation in due {
        pass.evaluated += 1;
        if let Err(err) = evaluate_one(work_db, now_epoch, dispatcher, &automation, &mut pass).await
        {
            tracing::warn!(
                automation_id = %automation.id,
                ?err,
                "automation scheduler: error evaluating automation; skipping",
            );
        }
    }

    pass
}

async fn evaluate_one(
    work_db: &WorkDb,
    now: i64,
    dispatcher: &dyn TriageDispatcher,
    automation: &Automation,
    pass: &mut AutomationSchedulerPass,
) -> anyhow::Result<()> {
    let AutomationTrigger::Schedule { cron, timezone } = &automation.trigger;

    let schedule = match parse_cron(cron) {
        Ok(schedule) => schedule,
        Err(err) => {
            tracing::warn!(automation_id = %automation.id, cron = %cron, %err, "invalid cron");
            pass.config_errors += 1;
            return Ok(());
        }
    };
    let tz = match parse_timezone(timezone) {
        Ok(tz) => tz,
        Err(err) => {
            tracing::warn!(automation_id = %automation.id, timezone = %timezone, %err, "invalid timezone");
            pass.config_errors += 1;
            return Ok(());
        }
    };

    // 1. Initialise next_due_at if unset (or unparseable).
    let next_due = match automation.next_due_at.as_deref().and_then(|s| s.parse::<i64>().ok()) {
        Some(next_due) => next_due,
        None => {
            match next_occurrence_after(&schedule, tz, now) {
                Some(next) => {
                    work_db.initialize_automation_next_due_at(&automation.id, next)?;
                    pass.initialized += 1;
                }
                None => tracing::warn!(
                    automation_id = %automation.id,
                    "no cron occurrence within scan horizon; leaving next_due_at unset",
                ),
            }
            return Ok(());
        }
    };

    // 2. Not actually due (list query is inclusive; guard against clock skew).
    if now < next_due {
        return Ok(());
    }

    // 3. Catch-up collapse: most_recent = latest occurrence <= now;
    //    following = first occurrence strictly after now.
    let mut most_recent = next_due;
    let mut following = next_occurrence_after(&schedule, tz, most_recent);
    let mut collapsed = 0u32;
    while let Some(f) = following {
        if f <= now && collapsed < MAX_CATCH_UP_COLLAPSE {
            most_recent = f;
            collapsed += 1;
            following = next_occurrence_after(&schedule, tz, most_recent);
        } else {
            break;
        }
    }

    let catch_up_window = automation
        .catch_up_window_secs
        .unwrap_or(DEFAULT_CATCH_UP_WINDOW_SECS);
    let staleness = now - most_recent;

    // 4. Skip-if-stale.
    if staleness > catch_up_window {
        let Some(advance_to) = following else {
            tracing::warn!(
                automation_id = %automation.id,
                "stale occurrence but no following occurrence within horizon; holding",
            );
            return Ok(());
        };
        // Don't relabel an occurrence we already attempted (a held
        // failed_will_retry): just advance past it. Its finalisation
        // (backoff → failed_gave_up) is Maint task 6.
        let already_attempted = work_db
            .automation_run_for_occurrence(&automation.id, most_recent)?
            .is_some();
        if already_attempted {
            work_db.record_automation_run_and_advance(
                AutomationFireRecord::builder()
                    .automation_id(automation.id.clone())
                    .scheduled_for(most_recent)
                    .started_at(now)
                    .outcome(AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
                    .detail("stale: catch-up window elapsed before retry")
                    .next_due_at(advance_to)
                    .build(),
            )?;
        } else {
            work_db.record_automation_run_and_advance(
                AutomationFireRecord::builder()
                    .automation_id(automation.id.clone())
                    .scheduled_for(most_recent)
                    .started_at(now)
                    .outcome(AUTOMATION_OUTCOME_SKIPPED)
                    .finished_at(now)
                    .detail(format!(
                        "stale catch-up: occurrence was {staleness}s late (> catch-up window {catch_up_window}s); advanced to next"
                    ))
                    .next_due_at(advance_to)
                    .build(),
            )?;
        }
        pass.skipped_stale += 1;
        return Ok(());
    }

    // 5. Open-task-limit gate.
    let open = work_db.count_open_tasks_for_automation(&automation.id)?;
    if open >= automation.open_task_limit {
        // Advance past the suppressed occurrence so a freshly-merged
        // automation doesn't fire its whole backlog at once. If there's no
        // following occurrence, hold (don't advance) rather than lose the slot.
        work_db.record_automation_run_and_advance(
            AutomationFireRecord::builder()
                .automation_id(automation.id.clone())
                .scheduled_for(most_recent)
                .started_at(now)
                .outcome(AUTOMATION_OUTCOME_SUPPRESSED_AT_LIMIT)
                .finished_at(now)
                .detail(format!(
                    "open-task count {open} at limit {}",
                    automation.open_task_limit
                ))
                .maybe_next_due_at(following)
                .build(),
        )?;
        pass.suppressed += 1;
        return Ok(());
    }

    // 6. Fire.
    match dispatcher.dispatch_triage(automation, most_recent).await {
        TriageDispatch::Dispatched { execution_id } => {
            work_db.record_automation_run_and_advance(
                AutomationFireRecord::builder()
                    .automation_id(automation.id.clone())
                    .scheduled_for(most_recent)
                    .started_at(now)
                    .outcome(AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
                    .triage_execution_id(execution_id)
                    .maybe_next_due_at(following)
                    .build(),
            )?;
            pass.fired += 1;
        }
        TriageDispatch::TransientFailure { detail } => {
            // Hold the occurrence (next_due_at unchanged) so it is retried.
            work_db.record_automation_run_and_advance(
                AutomationFireRecord::builder()
                    .automation_id(automation.id.clone())
                    .scheduled_for(most_recent)
                    .started_at(now)
                    .outcome(AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
                    .detail(detail)
                    .build(),
            )?;
            pass.held_transient += 1;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tempfile::TempDir;

    use super::*;
    use crate::automation_schedule::next_occurrence_after_str;
    use crate::work::{CreateChoreInput, CreateProductInput, WorkDb};
    use boss_protocol::{
        AUTOMATION_OUTCOME_FAILED_WILL_RETRY, AUTOMATION_OUTCOME_SKIPPED,
        AUTOMATION_OUTCOME_SUPPRESSED_AT_LIMIT, AutomationTrigger, CreateAutomationInput,
    };

    /// A dispatcher with a fixed verdict, recording every call.
    struct FakeDispatcher {
        verdict: TriageDispatch,
        calls: Mutex<Vec<(String, i64)>>,
    }

    impl FakeDispatcher {
        fn dispatched() -> Self {
            Self {
                verdict: TriageDispatch::Dispatched { execution_id: "exec_test".to_owned() },
                calls: Mutex::new(Vec::new()),
            }
        }
        fn transient() -> Self {
            Self {
                verdict: TriageDispatch::TransientFailure { detail: "vpn down".to_owned() },
                calls: Mutex::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl TriageDispatcher for FakeDispatcher {
        async fn dispatch_triage(&self, a: &Automation, scheduled_for: i64) -> TriageDispatch {
            self.calls.lock().unwrap().push((a.id.clone(), scheduled_for));
            self.verdict.clone()
        }
    }

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

    /// Create a daily-2pm-UTC automation. `open_task_limit` default 1.
    fn create_daily_automation(db: &WorkDb, product_id: &str) -> Automation {
        db.create_automation(
            CreateAutomationInput::builder()
                .product_id(product_id.to_owned())
                .name("daily")
                .trigger(AutomationTrigger::Schedule {
                    cron: "0 14 * * *".to_owned(),
                    timezone: "UTC".to_owned(),
                })
                .standing_instruction("do the thing")
                .build(),
        )
        .unwrap()
    }

    fn utc_epoch(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        use chrono::TimeZone;
        chrono::Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp()
    }

    /// An open task counted against the automation's cap.
    fn create_open_task_for_automation(db: &WorkDb, product_id: &str, automation_id: &str) {
        let task_id = db
            .create_chore(CreateChoreInput {
                product_id: product_id.to_owned(),
                name: "produced".to_owned(),
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
            .id;
        db.stamp_task_source_automation_for_test(&task_id, automation_id, "todo")
            .unwrap();
    }

    #[tokio::test]
    async fn first_evaluation_initializes_next_due_without_firing() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        assert!(automation.next_due_at.is_none());

        let now = utc_epoch(2026, 5, 28, 10, 0);
        let dispatcher = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, now, &dispatcher).await;

        assert_eq!(pass.initialized, 1);
        assert_eq!(pass.fired, 0);
        assert_eq!(dispatcher.call_count(), 0);

        let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
        let next: i64 = reloaded.next_due_at.unwrap().parse().unwrap();
        assert_eq!(next, utc_epoch(2026, 5, 28, 14, 0)); // today 2pm
        // No run recorded for an initialisation.
        assert!(db.list_automation_runs(&automation.id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn on_time_fire_dispatches_and_advances() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        // Park next_due at 2pm; fire 5s later.
        db.initialize_automation_next_due_at(&automation.id, utc_epoch(2026, 5, 28, 14, 0))
            .unwrap();
        let now = utc_epoch(2026, 5, 28, 14, 0) + 5;

        let dispatcher = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, now, &dispatcher).await;

        assert_eq!(pass.fired, 1, "{pass:?}");
        assert_eq!(dispatcher.call_count(), 1);

        let runs = db.list_automation_runs(&automation.id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, AUTOMATION_OUTCOME_FAILED_WILL_RETRY);
        assert_eq!(runs[0].triage_execution_id.as_deref(), Some("exec_test"));
        assert_eq!(
            runs[0].scheduled_for.parse::<i64>().unwrap(),
            utc_epoch(2026, 5, 28, 14, 0)
        );

        // next_due advanced to tomorrow 2pm.
        let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
        assert_eq!(
            reloaded.next_due_at.unwrap().parse::<i64>().unwrap(),
            utc_epoch(2026, 5, 29, 14, 0)
        );
        assert_eq!(reloaded.last_outcome.as_deref(), Some(AUTOMATION_OUTCOME_FAILED_WILL_RETRY));
    }

    #[tokio::test]
    async fn transient_failure_holds_occurrence() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, occ).unwrap();
        let now = occ + 5;

        let dispatcher = FakeDispatcher::transient();
        let pass = run_one_pass(&db, now, &dispatcher).await;

        assert_eq!(pass.held_transient, 1, "{pass:?}");
        let runs = db.list_automation_runs(&automation.id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, AUTOMATION_OUTCOME_FAILED_WILL_RETRY);
        assert!(runs[0].triage_execution_id.is_none());

        // next_due NOT advanced — occurrence is held.
        let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
        assert_eq!(reloaded.next_due_at.unwrap().parse::<i64>().unwrap(), occ);

        // A second pass re-attempts the SAME occurrence and upserts (no dup row).
        let pass2 = run_one_pass(&db, now + 30, &dispatcher).await;
        assert_eq!(pass2.held_transient, 1);
        let runs2 = db.list_automation_runs(&automation.id).unwrap();
        assert_eq!(runs2.len(), 1, "transient retry must upsert, not duplicate");
    }

    #[tokio::test]
    async fn suppressed_when_at_open_task_limit() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product); // limit 1
        create_open_task_for_automation(&db, &product, &automation.id);

        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, occ).unwrap();
        let now = occ + 5;

        let dispatcher = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, now, &dispatcher).await;

        assert_eq!(pass.suppressed, 1, "{pass:?}");
        assert_eq!(dispatcher.call_count(), 0, "must not dispatch while at cap");

        let runs = db.list_automation_runs(&automation.id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, AUTOMATION_OUTCOME_SUPPRESSED_AT_LIMIT);
        // Advanced past the suppressed occurrence.
        let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
        assert_eq!(
            reloaded.next_due_at.unwrap().parse::<i64>().unwrap(),
            utc_epoch(2026, 5, 29, 14, 0)
        );
    }

    #[tokio::test]
    async fn stale_occurrence_skipped_and_advanced() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        // next_due was 2 days ago; now is just before today's 2pm. The
        // most-recent occurrence (yesterday 2pm) is >24h stale.
        db.initialize_automation_next_due_at(&automation.id, utc_epoch(2026, 5, 26, 14, 0))
            .unwrap();
        let now = utc_epoch(2026, 5, 28, 13, 0);

        let dispatcher = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, now, &dispatcher).await;

        assert_eq!(pass.skipped_stale, 1, "{pass:?}");
        assert_eq!(dispatcher.call_count(), 0);

        let runs = db.list_automation_runs(&automation.id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, AUTOMATION_OUTCOME_SKIPPED);
        // Advanced to today's 2pm (the next future occurrence).
        let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
        assert_eq!(
            reloaded.next_due_at.unwrap().parse::<i64>().unwrap(),
            utc_epoch(2026, 5, 28, 14, 0)
        );
    }

    #[tokio::test]
    async fn slightly_late_wake_catches_up_within_window() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, occ).unwrap();
        // Woke 10 minutes late — within the 15-minute window → fire (catch up).
        let now = occ + 10 * 60;

        let dispatcher = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, now, &dispatcher).await;

        assert_eq!(pass.fired, 1, "{pass:?}");
        assert_eq!(
            db.list_automation_runs(&automation.id).unwrap()[0]
                .scheduled_for
                .parse::<i64>()
                .unwrap(),
            occ,
            "must fire the missed occurrence, not skip it"
        );
    }

    #[tokio::test]
    async fn high_frequency_outage_collapses_to_most_recent() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = db
            .create_automation(
                CreateAutomationInput::builder()
                    .product_id(product.clone())
                    .name("every-5-min")
                    .trigger(AutomationTrigger::Schedule {
                        cron: "*/5 * * * *".to_owned(),
                        timezone: "UTC".to_owned(),
                    })
                    .standing_instruction("x")
                    .build(),
            )
            .unwrap();
        // next_due 14:00; asleep until 14:32. Occurrences 14:00..14:30 missed.
        db.initialize_automation_next_due_at(&automation.id, utc_epoch(2026, 5, 28, 14, 0))
            .unwrap();
        let now = utc_epoch(2026, 5, 28, 14, 32);

        let dispatcher = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, now, &dispatcher).await;

        // Fires exactly once, for 14:30 (most recent within window), not 7x.
        assert_eq!(pass.fired, 1, "{pass:?}");
        assert_eq!(dispatcher.call_count(), 1);
        let runs = db.list_automation_runs(&automation.id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].scheduled_for.parse::<i64>().unwrap(),
            utc_epoch(2026, 5, 28, 14, 30)
        );
        // Advanced to 14:35.
        let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
        assert_eq!(
            reloaded.next_due_at.unwrap().parse::<i64>().unwrap(),
            utc_epoch(2026, 5, 28, 14, 35)
        );
    }

    #[tokio::test]
    async fn disabled_automation_is_not_evaluated() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        db.initialize_automation_next_due_at(&automation.id, utc_epoch(2026, 5, 28, 14, 0))
            .unwrap();
        db.disable_automation(&automation.id).unwrap();

        let dispatcher = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, utc_epoch(2026, 5, 28, 15, 0), &dispatcher).await;
        assert_eq!(pass.evaluated, 0);
        assert_eq!(dispatcher.call_count(), 0);
    }

    #[tokio::test]
    async fn not_due_automation_does_nothing() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        db.initialize_automation_next_due_at(&automation.id, utc_epoch(2026, 5, 28, 14, 0))
            .unwrap();
        // now is before next_due → list_due_automations must not return it.
        let dispatcher = FakeDispatcher::dispatched();
        let pass = run_one_pass(&db, utc_epoch(2026, 5, 28, 13, 0), &dispatcher).await;
        assert_eq!(pass.evaluated, 0);
    }

    /// End-to-end of the math + scheduler: park next_due via the same
    /// occurrence function the scheduler uses, fire, and confirm the advance
    /// matches the next computed occurrence.
    #[tokio::test]
    async fn advance_matches_computed_next_occurrence() {
        let (_d, db) = open_db();
        let product = create_product(&db);
        let automation = create_daily_automation(&db, &product);
        let occ = utc_epoch(2026, 5, 28, 14, 0);
        db.initialize_automation_next_due_at(&automation.id, occ).unwrap();

        let dispatcher = FakeDispatcher::dispatched();
        run_one_pass(&db, occ + 1, &dispatcher).await;

        let expected_following = next_occurrence_after_str("0 14 * * *", "UTC", occ).unwrap().unwrap();
        let reloaded = db.get_automation(&automation.id).unwrap().unwrap();
        assert_eq!(reloaded.next_due_at.unwrap().parse::<i64>().unwrap(), expected_following);
    }
}
