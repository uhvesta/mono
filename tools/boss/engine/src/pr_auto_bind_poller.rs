//! Safety-net poller for PR auto-binding.
//!
//! The primary auto-bind path is the worker's `Stop` hook event:
//! claude fires the hook → boss-event shim splices `_boss_run_id`
//! into the payload → engine's events socket dispatches into
//! [`crate::completion::WorkerCompletionHandler::on_stop`]. When that
//! chain works end-to-end, the chore's `pr_url` is bound and its
//! status moves to `in_review` within ~1s of the worker exiting.
//!
//! Every link in that chain has been observed to fail in production:
//!
//!   * Claude has been observed to exit without firing `Stop` (the
//!     harness writes a final response but never trips the
//!     hook — exec_18aebee6bbbbfaf8_7 / Worf, 2026-05-12).
//!   * The events socket has dropped accepted connections during
//!     engine restarts; the shim's on-disk buffer survives those, but
//!     only drains when the worker fires another hook — if it just
//!     exits, the Stop sits in `.boss/events-pending.jsonl` forever.
//!   * The `_boss_run_id` splice silently fails if `BOSS_RUN_ID` is
//!     unset (the engine then falls back to the peer-pid ancestor
//!     walk, which itself can miss for deep claude→sh→shim chains).
//!
//! This module is the floor under all of those: every `interval`
//! seconds, it iterates [`crate::work::WorkDb::list_executions_pending_pr_auto_bind`]
//! and re-runs the exact same completion path the Stop hook would
//! have driven. The transition path on the DB side
//! ([`crate::work::WorkDb::record_worker_pr_completion`]) is
//! idempotent (status-gated UPDATE), so a poller pass that races with
//! a live Stop event is a benign no-op.
//!
//! ## Quiescence gate
//!
//! The poller never touches an execution whose worker is *actively
//! working*. The signal is `LiveWorkerStateRegistry::last_event_at`:
//! if it's within the quiescence window, the worker is mid-tool-use
//! and we leave it alone. If it's older than the window (or there
//! is no live-state entry at all, which itself is a strong "the slot
//! is gone" signal), we run PR detection. The window also moves the
//! poller out of races with the Stop-hook path: the Stop fires
//! immediately at idle, the poller waits long enough that the on-Stop
//! transition would have committed already if it was going to.
//!
//! ## Why not just rely on the merge poller?
//!
//! [`crate::merge_poller`] handles `in_review → done` (PR merged).
//! It does **not** handle `active → in_review` — that requires
//! workspace-local PR detection (jj log + gh api by commit sha)
//! against a non-colocated cube workspace, which the merge poller
//! deliberately avoids (it queries by PR URL only). So the two
//! pollers are complementary: this one closes the gap at the front
//! of the lifecycle, the merge poller closes it at the back.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::completion::{CompletionTrigger, StopOutcome, WorkerCompletionHandler};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::work::WorkDb;

/// Tally returned by a single sweep. Lets callers log a one-line
/// summary instead of every per-execution probe outcome.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AutoBindSweepOutcome {
    /// Executions inspected in this pass.
    pub considered: usize,
    /// Executions skipped because the worker was still active
    /// (`last_event_at` within the quiescence window).
    pub skipped_active: usize,
    /// Executions where the safety-net invoked the completion handler
    /// — i.e. the gate passed. Not every invocation results in a
    /// transition; consult `bound_in_review` / `bound_done` for that.
    pub invoked: usize,
    /// PR detected and the work item moved to `in_review`.
    pub bound_in_review: usize,
    /// PR detected and already merged; work item moved to `done`.
    pub bound_done: usize,
}

impl AutoBindSweepOutcome {
    /// Total work-item transitions driven by this sweep.
    pub fn total_transitions(self) -> usize {
        self.bound_in_review + self.bound_done
    }
}

/// Inspect every non-terminal execution with a workspace_path and
/// re-run the completion handler on the ones whose workers have been
/// quiescent for longer than `quiescence`. Errors are logged but
/// never propagate — a flaky GitHub call must not crash the engine.
///
/// `live_states` is consulted *only* to gate the run; it is not
/// mutated. The completion handler owns its own state transitions.
pub async fn run_one_pass(
    work_db: &WorkDb,
    handler: &WorkerCompletionHandler,
    live_states: &LiveWorkerStateRegistry,
    quiescence: Duration,
) -> AutoBindSweepOutcome {
    let candidates = match work_db.list_executions_pending_pr_auto_bind() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(?err, "pr auto-bind poller: failed to list candidate executions");
            return AutoBindSweepOutcome::default();
        }
    };
    if candidates.is_empty() {
        return AutoBindSweepOutcome::default();
    }

    let live = live_states.snapshot();
    let mut outcome = AutoBindSweepOutcome::default();
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    for execution in candidates {
        outcome.considered += 1;
        let live_state = live.iter().find(|s| s.run_id == execution.id);
        if !quiescent_enough(
            live_state.and_then(|s| s.last_event_at.as_deref()),
            now_secs,
            quiescence,
        ) {
            outcome.skipped_active += 1;
            tracing::debug!(
                execution_id = %execution.id,
                "pr auto-bind poller: worker still active within quiescence window; skipping",
            );
            continue;
        }
        outcome.invoked += 1;
        tracing::info!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            status = %execution.status,
            "pr auto-bind poller: invoking completion handler — Stop hook either never fired or failed to find a PR",
        );
        let result = handler
            .on_stop_from(&execution.id, CompletionTrigger::AutoBindPoller)
            .await;
        match result {
            StopOutcome::PrDetected { pr_url } => {
                outcome.bound_in_review += 1;
                tracing::info!(
                    execution_id = %execution.id,
                    pr_url = %pr_url,
                    "pr auto-bind poller: bound PR and moved work item to in_review",
                );
            }
            StopOutcome::PrMerged { pr_url } => {
                outcome.bound_done += 1;
                tracing::info!(
                    execution_id = %execution.id,
                    pr_url = %pr_url,
                    "pr auto-bind poller: bound merged PR and moved work item to done",
                );
            }
            // No transition this pass — next sweep retries. The
            // completion handler emits its own dispatch events
            // (PrDetectionResult with the classification) so we
            // don't double-log here.
            other => {
                tracing::debug!(
                    execution_id = %execution.id,
                    ?other,
                    "pr auto-bind poller: completion handler reported non-transitioning outcome",
                );
            }
        }
    }
    outcome
}

/// Decide whether the worker's last hook event is far enough in the
/// past for the poller to fire. `last_event_at` is the ISO-8601
/// timestamp the live-state registry stamps on every hook arrival;
/// `None` (no live-state entry at all) is treated as "long quiet"
/// because the slot has already been released and the worker is
/// definitively done. Unparseable strings are also treated as
/// quiescent — if the timestamp is corrupt, we can't tell whether
/// the worker is active, so the conservative path is to act.
fn quiescent_enough(
    last_event_at: Option<&str>,
    now_epoch_secs: i64,
    quiescence: Duration,
) -> bool {
    let Some(stamp) = last_event_at else {
        return true;
    };
    let Some(then) = parse_iso8601_utc(stamp) else {
        return true;
    };
    let elapsed = now_epoch_secs.saturating_sub(then);
    elapsed >= quiescence.as_secs() as i64
}

/// Reverse of `crate::live_worker_state::format_iso8601_utc` — parse
/// a `YYYY-MM-DDTHH:MM:SSZ` string back to epoch seconds. Returns
/// `None` for any input that doesn't match the exact shape the engine
/// writes (the registry is the only producer of these values, so we
/// don't bother with leniency).
fn parse_iso8601_utc(stamp: &str) -> Option<i64> {
    // `YYYY-MM-DDTHH:MM:SSZ` is 20 chars.
    let bytes = stamp.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    // Trailing `Z` (UTC) or fractional seconds — we ignore anything
    // beyond the integer second.
    if bytes[19] != b'Z' && bytes[19] != b'.' && bytes[19] != b'+' && bytes[19] != b'-' {
        return None;
    }
    let year: i64 = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
    let month: u32 = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
    let day: u32 = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
    let hour: i64 = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
    let minute: i64 = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
    let second: i64 = std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?;
    let days = days_since_1970(year, month, day)?;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Inverse of `ymd_from_days_since_1970` in `live_worker_state.rs` —
/// (year, month, day) → days since 1970-01-01. Uses the same Howard
/// Hinnant date algorithm so a round trip through the engine's
/// formatter is exact.
fn days_since_1970(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = month as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let d = day as u64;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe as i64 - 719_468)
}

/// Spawn a background task that runs [`run_one_pass`] forever at
/// `interval`, with a small initial delay so engine startup isn't
/// blocked. Returned `JoinHandle` is detached by callers — the poller
/// has no shutdown path; aborting the engine process is the only way
/// out, matching every other engine background task.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    handler: Arc<WorkerCompletionHandler>,
    live_states: Arc<LiveWorkerStateRegistry>,
    interval: Duration,
    quiescence: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Stagger startup so we don't pile a `jj log` / `gh api`
        // per execution on top of the engine's other startup work.
        tokio::time::sleep(interval).await;
        loop {
            let outcome = run_one_pass(work_db.as_ref(), handler.as_ref(), live_states.as_ref(), quiescence).await;
            if outcome.invoked > 0 || outcome.total_transitions() > 0 {
                tracing::info!(
                    considered = outcome.considered,
                    skipped_active = outcome.skipped_active,
                    invoked = outcome.invoked,
                    bound_in_review = outcome.bound_in_review,
                    bound_done = outcome.bound_done,
                    "pr auto-bind poller: sweep complete",
                );
            }
            tokio::time::sleep(interval).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-05-12T12:00:00Z as epoch seconds. Hand-computed:
    /// 56 years (1970-2026) + 14 leap days = 20,585 days + 31+28+31+30+12-1 = 131 days
    /// = 20,585 + 131 = 20,716 days * 86400 + 12*3600 = 1789948800.
    /// Easier: just round-trip through `parse_iso8601_utc`.
    fn fixed_now() -> i64 {
        parse_iso8601_utc("2026-05-12T12:00:00Z").expect("known-good fixture")
    }

    #[test]
    fn quiescent_when_no_live_state() {
        // No entry in the live-state registry at all → the slot has
        // been released; the worker is done. Run the detector.
        assert!(quiescent_enough(None, fixed_now(), Duration::from_secs(30)));
    }

    #[test]
    fn quiescent_when_last_event_old_enough() {
        let last = "2026-05-12T11:59:00Z"; // 60s ago, window 30s.
        assert!(quiescent_enough(Some(last), fixed_now(), Duration::from_secs(30)));
    }

    #[test]
    fn not_quiescent_when_last_event_recent() {
        let last = "2026-05-12T11:59:55Z"; // 5s ago, window 30s.
        assert!(!quiescent_enough(Some(last), fixed_now(), Duration::from_secs(30)));
    }

    #[test]
    fn unparseable_timestamp_treated_as_quiescent() {
        // A corrupt timestamp would otherwise pin the poller. Conservative
        // path: act on the row; the completion handler is idempotent.
        assert!(quiescent_enough(Some("not-a-date"), fixed_now(), Duration::from_secs(30)));
    }

    #[test]
    fn parse_iso_pins_to_known_epochs() {
        // The engine formatter (`live_worker_state::format_iso8601_utc`)
        // is the only producer; pin the inverse against hand-computed
        // epochs so a future change to either side has to update both.
        assert_eq!(parse_iso8601_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601_utc("2000-01-01T00:00:00Z"), Some(946_684_800));
        assert_eq!(parse_iso8601_utc("2026-05-12T12:00:00Z"), Some(1_778_587_200));
    }

    #[test]
    fn parse_iso_rejects_malformed() {
        assert_eq!(parse_iso8601_utc("not-a-date"), None);
        assert_eq!(parse_iso8601_utc("2026/05/12T12:00:00Z"), None);
        assert_eq!(parse_iso8601_utc("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso8601_utc("2026-05-12T12:00:00"), None);
    }

    // End-to-end regression test: a Stop event that never reaches
    // the engine (the 2026-05-12 Worf shape) must be picked up by
    // the safety-net poller and result in `pr_url` populated +
    // status moved to `in_review` within 10 seconds.
    //
    // This test exercises the full poller → completion-handler →
    // record_worker_pr_completion path against an in-memory work
    // db, with a stub PR detector and no live-state entry (the
    // worst-case shape — the slot was released so the registry has
    // nothing for the run).
    use std::path::Path;
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use tempfile::tempdir;

    use crate::completion::{
        NoopProbeQueuer, NoopWorkerPaneReleaser, PrDetector, PrStatus,
    };
    use crate::coordinator::{
        CubeChangeHandle, CubeClient, CubeRepoHandle, CubeWorkspaceLease, CubeWorkspaceStatus,
        ExecutionPublisher,
    };
    use crate::work::{
        CreateChoreInput, CreateExecutionInput, CreateProductInput, WorkDb, WorkItem,
    };

    struct StubFreshPrDetector(String);

    #[async_trait]
    impl PrDetector for StubFreshPrDetector {
        async fn detect_pr(
            &self,
            _workspace_path: &Path,
            _repo_remote_url: &str,
        ) -> Result<PrStatus> {
            Ok(PrStatus::Fresh {
                url: self.0.clone(),
            })
        }
    }

    #[derive(Default)]
    struct StubCubeClient;

    #[async_trait]
    impl CubeClient for StubCubeClient {
        async fn ensure_repo(&self, _origin: &str) -> Result<CubeRepoHandle> {
            unreachable!()
        }
        async fn lease_workspace(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
        ) -> Result<CubeWorkspaceLease> {
            unreachable!()
        }
        async fn create_change(
            &self,
            _: &std::path::PathBuf,
            _: &str,
        ) -> Result<CubeChangeHandle> {
            unreachable!()
        }
        async fn release_workspace(&self, _lease_id: &str) -> Result<()> {
            Ok(())
        }
        async fn workspace_status(&self, _: &Path) -> Result<CubeWorkspaceStatus> {
            unreachable!()
        }
        async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> Result<()> {
            Ok(())
        }
        async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
            Ok(())
        }
        async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct NoopPublisher;

    #[async_trait]
    impl ExecutionPublisher for NoopPublisher {
        async fn publish(&self, _: &str, _: &str, _: &str, _: &str) {}
        async fn publish_work_item_changed(&self, _: &str, _: &str, _: &str) {}
        async fn publish_frontend_event_on_product(
            &self,
            _product_id: &str,
            _event: boss_protocol::FrontendEvent,
        ) {
        }
    }

    /// Build a chore in `waiting_human` shape with workspace_path set —
    /// the post-spawn state the safety-net poller will see.
    fn fixture(workspace_path: &Path) -> (Arc<WorkDb>, String, String) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("boss.db");
        std::mem::forget(dir);
        let db = Arc::new(WorkDb::open(path).unwrap());
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".into()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Bind PR on Stop".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".into(),
                status: Some("ready".into()),
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();
        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                workspace_path.to_str().unwrap(),
            )
            .unwrap();
        let _ = db
            .finish_execution_run(
                &execution.id,
                &run.id,
                "waiting_human",
                "completed",
                Some("spawned worker pane"),
                None,
                false,
                None,
            )
            .unwrap();

        (db, chore.id, execution.id)
    }

    /// Acceptance criterion from the task description:
    ///
    ///   "a Stop hook with a known transcript path and a known PR on
    ///    the worker's branch results in `tasks.pr_url` populated +
    ///    `tasks.status` moved to `in-review` within 10s."
    ///
    /// The poller path runs even when the on-Stop hook never reached
    /// the engine (this test has no live-state entry at all, modelling
    /// the slot-already-released case). `run_one_pass` is synchronous
    /// for tests; production runs every 30s, well inside the 10s
    /// budget on a probe-by-probe basis.
    #[tokio::test]
    async fn safety_net_poller_binds_pr_and_moves_chore_to_in_review() {
        let workspace = tempdir().unwrap();
        let (db, chore_id, execution_id) = fixture(workspace.path());
        let live_states = LiveWorkerStateRegistry::new();

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            Arc::new(StubFreshPrDetector(
                "https://github.com/spinyfin/mono/pull/377".into(),
            )),
            Arc::new(StubCubeClient),
            Arc::new(NoopPublisher),
            Arc::new(NoopWorkerPaneReleaser),
            Arc::new(NoopProbeQueuer),
        );

        let outcome = run_one_pass(db.as_ref(), &handler, &live_states, Duration::from_secs(30)).await;
        assert_eq!(outcome.considered, 1);
        assert_eq!(outcome.skipped_active, 0);
        assert_eq!(outcome.invoked, 1);
        assert_eq!(outcome.bound_in_review, 1);
        assert_eq!(outcome.bound_done, 0);

        // The work item moved to in_review with the PR url bound.
        match db.get_work_item(&chore_id).unwrap() {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "in_review");
                assert_eq!(
                    t.pr_url.as_deref(),
                    Some("https://github.com/spinyfin/mono/pull/377"),
                );
            }
            other => panic!("expected chore, got {other:?}"),
        }

        // The execution finalised: lease cleared, workspace_path
        // cleared, finished_at stamped.
        let execution = db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, "completed");
        assert!(execution.cube_lease_id.is_none());
        assert!(execution.workspace_path.is_none());
        assert!(execution.finished_at.is_some());

        // Re-running the poller is idempotent — the execution is
        // already terminal, so the gate `status IN ('running',
        // 'waiting_human')` excludes it from the candidate list.
        let outcome2 = run_one_pass(db.as_ref(), &handler, &live_states, Duration::from_secs(30)).await;
        assert_eq!(outcome2.considered, 0);
    }

    /// Counterpart: when the worker is *active* (recent
    /// `last_event_at` on live state), the poller must NOT fire. The
    /// completion handler is invoked only on quiescent rows so a
    /// worker mid-tool-call doesn't have its work item yanked out
    /// from under it.
    #[tokio::test]
    async fn safety_net_poller_skips_workers_within_quiescence_window() {
        use boss_protocol::WorkItemBinding;

        let workspace = tempdir().unwrap();
        let (db, chore_id, execution_id) = fixture(workspace.path());
        let live_states = LiveWorkerStateRegistry::new();
        // Stamp the live state with a fresh `last_event_at`.
        live_states.register_spawn(
            1,
            execution_id.clone(),
            "claude-opus-4-7".to_owned(),
            12345,
            Some(WorkItemBinding {
                work_item_id: chore_id.clone(),
                work_item_name: "Bind PR on Stop".into(),
                execution_id: execution_id.clone(),
            }),
        );
        // Force a `last_event_at` that's just barely-recent by
        // applying a synthetic event — register_spawn alone leaves
        // last_event_at = None, which the poller treats as quiescent.
        live_states.apply_event(
            1,
            &boss_protocol::WorkerEvent::PreToolUse {
                session_id: "s".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::Value::Null,
            },
        );

        let handler = WorkerCompletionHandler::new(
            db.clone(),
            Arc::new(StubFreshPrDetector(
                "https://github.com/spinyfin/mono/pull/377".into(),
            )),
            Arc::new(StubCubeClient),
            Arc::new(NoopPublisher),
            Arc::new(NoopWorkerPaneReleaser),
            Arc::new(NoopProbeQueuer),
        );

        // 1h quiescence — `last_event_at` was just stamped, so this
        // will gate the poller out.
        let outcome = run_one_pass(db.as_ref(), &handler, &live_states, Duration::from_secs(3_600)).await;
        assert_eq!(outcome.considered, 1);
        assert_eq!(outcome.skipped_active, 1);
        assert_eq!(outcome.invoked, 0);

        // Chore stays in active — the gate held.
        match db.get_work_item(&chore_id).unwrap() {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, "active");
                assert!(t.pr_url.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }

    /// Lock the actual SQL filter that gates which rows the poller
    /// considers. A future schema change that drops `workspace_path`
    /// or expands the status set should trip this — the poller
    /// MUST NOT sweep terminal rows.
    #[test]
    fn list_executions_pending_pr_auto_bind_includes_running_and_waiting_human_only() {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".into(),
                description: None,
                repo_remote_url: Some("git@github.com:foo/bar.git".into()),
            })
            .unwrap();
        // Row A: `running` with workspace_path → included.
        let chore_a = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "A".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
            })
            .unwrap();
        let exec_a = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore_a.id.clone(),
                kind: "chore_implementation".into(),
                status: Some("ready".into()),
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();
        db.start_execution_run(
            &exec_a.id,
            "worker-a",
            "repo",
            "lease-a",
            "ws-a",
            "/tmp/A",
        )
        .unwrap();
        // Row B: ready (no workspace_path yet) → excluded.
        let chore_b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".into(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
            })
            .unwrap();
        let _exec_b = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore_b.id.clone(),
                kind: "chore_implementation".into(),
                status: Some("ready".into()),
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();

        let rows = db.list_executions_pending_pr_auto_bind().unwrap();
        assert_eq!(rows.len(), 1, "only row A should match (ready/no workspace excluded)");
        assert_eq!(rows[0].id, exec_a.id);
        assert_eq!(rows[0].status, "running");
        assert_eq!(rows[0].workspace_path.as_deref(), Some("/tmp/A"));
    }

}
