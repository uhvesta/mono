//! Engine-startup reconciliation of persisted in-flight runs.
//!
//! When the engine restarts it has no in-memory live-worker state: the
//! [`crate::live_worker_state::LiveWorkerStateRegistry`] is empty until
//! workers send their first hook event. Without a separate signal,
//! [`crate::work::WorkDb::reconcile_active_dispatch`] would treat every
//! non-terminal `work_executions` row as stale and redispatch the work
//! item — spawning a *second* worker on top of the first one, which is
//! exactly what produced the duplicate-dispatch incident on 2026-05-07
//! (slot 1+7 and slot 4+8 each running the same chore).
//!
//! This module probes the **cube workspace lease state** to decide,
//! per persisted in-flight execution, whether the underlying worker is
//! still alive. The events socket is intentionally NOT consulted —
//! that socket can itself be broken at restart time (it was on
//! 2026-05-07), so we use cube state as an independent oracle.
//!
//! Verdicts:
//!
//! - [`RunReconcileVerdict::Live`]: cube reports the workspace is
//!   `leased` with the same `lease_id` we recorded, and the lease has
//!   not expired. The dispatcher SKIPS this work item — no second
//!   worker.
//! - [`RunReconcileVerdict::Dead`]: cube reports the workspace is
//!   `free`, OR the lease_id has changed (someone else holds it now),
//!   OR the lease has explicitly expired. The execution row is treated
//!   as stale; `reconcile_active_dispatch` will mark it `abandoned`
//!   and create a fresh `ready` row for the dispatcher.
//! - [`RunReconcileVerdict::Unknown`]: the cube call failed, the
//!   workspace isn't in cube's snapshot at all, or the persisted row
//!   is missing the lease/workspace metadata we need to make a
//!   judgement. Treated like `Live` for dispatch purposes (no second
//!   worker) but logged loudly so a human can resolve.
//!
//! The scope is deliberately conservative — see the work-item brief
//! ("Don't aggressively kill old workers on engine startup. They may
//! be doing real work that just lost its event channel. Reconcile,
//! don't reap.").

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use boss_protocol::WorkExecution;

use crate::coordinator::{CubeClient, CubeWorkspaceStatus};

/// Per-execution outcome of the startup probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunReconcileVerdict {
    /// Worker is presumed still attached; dispatch should not
    /// redispatch the work item.
    Live,
    /// Worker is presumed gone; the execution row is stale and the
    /// work item should be redispatched after the row is marked
    /// abandoned by the existing reconcile transaction.
    Dead,
    /// Insufficient signal to decide. Conservatively treated as Live
    /// at dispatch time so we don't risk a duplicate, but surfaced via
    /// `tracing::warn!` so an operator can resolve manually.
    Unknown,
}

/// Output of [`probe_in_flight_runs`].
#[derive(Debug, Clone, Default)]
pub struct RunReconcileReport {
    /// Verdict per `work_executions.id` we were asked to probe. Rows
    /// that produce any verdict appear here; rows we couldn't probe
    /// at all (e.g. missing `cube_lease_id`) are absent.
    pub verdicts: HashMap<String, RunReconcileVerdict>,
    pub live_count: usize,
    pub dead_count: usize,
    pub unknown_count: usize,
}

impl RunReconcileReport {
    /// Iterator over execution ids the dispatcher must NOT redispatch
    /// (Live ∪ Unknown). Convenient for plumbing into the
    /// `is_live` predicate that
    /// [`crate::work::WorkDb::reconcile_active_dispatch`] expects.
    pub fn skip_dispatch_ids(&self) -> impl Iterator<Item = &str> {
        self.verdicts.iter().filter_map(|(id, verdict)| match verdict {
            RunReconcileVerdict::Live | RunReconcileVerdict::Unknown => Some(id.as_str()),
            RunReconcileVerdict::Dead => None,
        })
    }
}

/// Probe `cube workspace list` once and decide the verdict for every
/// in-flight execution against that snapshot. `now_epoch_s` is plumbed
/// in so unit tests can pin a deterministic clock; production callers
/// should use [`current_epoch_s`].
pub async fn probe_in_flight_runs(
    cube: &dyn CubeClient,
    in_flight: &[WorkExecution],
    now_epoch_s: i64,
) -> RunReconcileReport {
    let mut report = RunReconcileReport::default();
    if in_flight.is_empty() {
        return report;
    }

    // One snapshot for all rows. `cube workspace list` is a sqlite
    // read — cheap, atomic, and avoids N round-trips. If it fails we
    // bail to "every row is Unknown" rather than smearing N separate
    // errors across the log.
    let snapshot = match cube.list_workspaces().await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(
                error = format!("{err:#}"),
                in_flight_count = in_flight.len(),
                "cube workspace list failed during startup reconcile; treating every persisted in-flight run as Unknown — operator should investigate before relying on auto-dispatch"
            );
            for execution in in_flight {
                record(&mut report, execution.id.clone(), RunReconcileVerdict::Unknown);
            }
            return report;
        }
    };

    // Index cube's snapshot by workspace_id so we don't re-scan the
    // vector for each in-flight row. Production fleets stay well under
    // the hard cap of 8 workspaces, but the indirection is cheap and
    // keeps the loop O(in_flight).
    let by_workspace_id: HashMap<&str, &CubeWorkspaceStatus> = snapshot
        .iter()
        .map(|w| (w.workspace_id.as_str(), w))
        .collect();

    for execution in in_flight {
        let verdict = classify(execution, &by_workspace_id, now_epoch_s);
        record(&mut report, execution.id.clone(), verdict);
    }
    report
}

fn classify(
    execution: &WorkExecution,
    by_workspace_id: &HashMap<&str, &CubeWorkspaceStatus>,
    now_epoch_s: i64,
) -> RunReconcileVerdict {
    let Some(cube_lease_id) = execution.cube_lease_id.as_deref() else {
        // No lease ever recorded — the row is non-terminal but never
        // reached `start_execution_run`. We can't probe; the existing
        // pre-reconcile sweep should have caught this, but if it
        // didn't, surface as Unknown rather than silently
        // redispatching.
        tracing::warn!(
            execution_id = %execution.id,
            "in-flight execution has no cube_lease_id; cannot probe — treating as Unknown"
        );
        return RunReconcileVerdict::Unknown;
    };
    let Some(cube_workspace_id) = execution.cube_workspace_id.as_deref() else {
        tracing::warn!(
            execution_id = %execution.id,
            cube_lease_id,
            "in-flight execution has no cube_workspace_id; cannot probe — treating as Unknown"
        );
        return RunReconcileVerdict::Unknown;
    };

    let Some(workspace) = by_workspace_id.get(cube_workspace_id) else {
        // Cube doesn't know about this workspace. Could be a stale row
        // pointing at a workspace that has since been removed, or a
        // race where cube hasn't yet surfaced it. Either way: not safe
        // to redispatch automatically.
        tracing::warn!(
            execution_id = %execution.id,
            cube_workspace_id,
            cube_lease_id,
            "cube snapshot does not list the persisted workspace_id; treating as Unknown"
        );
        return RunReconcileVerdict::Unknown;
    };

    // State must be `leased` with a matching `lease_id` for the worker
    // to be presumed alive. Anything else is the worker's lease being
    // gone (released, force-released, expired, or replaced).
    let lease_active = workspace.state == "leased"
        && workspace.lease_id.as_deref() == Some(cube_lease_id);
    if !lease_active {
        tracing::info!(
            execution_id = %execution.id,
            cube_workspace_id,
            cube_lease_id,
            cube_state = %workspace.state,
            cube_lease_id_now = workspace.lease_id.as_deref().unwrap_or("<none>"),
            "cube reports lease is no longer active; reconciling as Dead"
        );
        return RunReconcileVerdict::Dead;
    }

    // Lease still bound to our id. Check the TTL: cube assigns every
    // lease a `lease_expires_at_epoch_s` and may not have reaped an
    // expired lease yet (no janitor runs at lease-list time). If our
    // recorded lease has logically expired, the worker has been gone
    // long enough that auto-redispatch is safer than waiting for the
    // next dispatcher tick.
    if let Some(expires_at) = workspace.lease_expires_at_epoch_s {
        if expires_at <= now_epoch_s {
            tracing::info!(
                execution_id = %execution.id,
                cube_workspace_id,
                cube_lease_id,
                expires_at,
                now_epoch_s,
                "cube reports lease has logically expired (TTL); reconciling as Dead"
            );
            return RunReconcileVerdict::Dead;
        }
    }

    RunReconcileVerdict::Live
}

fn record(report: &mut RunReconcileReport, id: String, verdict: RunReconcileVerdict) {
    match verdict {
        RunReconcileVerdict::Live => report.live_count += 1,
        RunReconcileVerdict::Dead => report.dead_count += 1,
        RunReconcileVerdict::Unknown => report.unknown_count += 1,
    }
    report.verdicts.insert(id, verdict);
}

/// Production wall-clock helper for the probe. Tests pass a fixed
/// epoch directly to [`probe_in_flight_runs`] so they don't depend on
/// the system clock.
pub fn current_epoch_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use crate::coordinator::{
        CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease,
        CubeWorkspaceStatus,
    };

    struct StubCube {
        list_response: Mutex<Result<Vec<CubeWorkspaceStatus>>>,
    }

    impl StubCube {
        fn ok(rows: Vec<CubeWorkspaceStatus>) -> Self {
            Self {
                list_response: Mutex::new(Ok(rows)),
            }
        }

        fn err(message: &str) -> Self {
            let msg = message.to_owned();
            Self {
                list_response: Mutex::new(Err(anyhow!(msg))),
            }
        }
    }

    #[async_trait]
    impl CubeClient for StubCube {
        async fn ensure_repo(&self, _origin: &str) -> Result<CubeRepoHandle> {
            unimplemented!("not used by probe")
        }

        async fn lease_workspace(
            &self,
            _repo_id: &str,
            _task: &str,
            _prefer: Option<&str>,
        ) -> Result<CubeWorkspaceLease> {
            unimplemented!("not used by probe")
        }

        async fn create_change(
            &self,
            _workspace_path: &PathBuf,
            _title: &str,
        ) -> Result<CubeChangeHandle> {
            unimplemented!("not used by probe")
        }

        async fn release_workspace(&self, _lease_id: &str) -> Result<()> {
            unimplemented!("not used by probe")
        }

        async fn workspace_status(&self, _workspace_path: &Path) -> Result<CubeWorkspaceStatus> {
            unimplemented!("not used by probe")
        }

        async fn heartbeat_lease(&self, _lease_id: &str, _ttl: Option<u64>) -> Result<()> {
            unimplemented!("not used by probe")
        }

        async fn force_release_lease(
            &self,
            _lease_id: &str,
            _reason: Option<&str>,
        ) -> Result<()> {
            unimplemented!("not used by probe")
        }

        async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
            // Take ownership of the canned response — Result isn't
            // Clone, and tests fire one probe per StubCube.
            let mut guard = self.list_response.lock().unwrap();
            std::mem::replace(&mut *guard, Err(anyhow!("StubCube already drained")))
        }

        async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
            unimplemented!("not used by probe")
        }
    }

    fn execution(id: &str, lease_id: &str, workspace_id: &str) -> WorkExecution {
        WorkExecution {
            id: id.to_owned(),
            work_item_id: format!("task-{id}"),
            kind: "chore_implementation".to_owned(),
            status: "running".to_owned(),
            repo_remote_url: "git@example.com:foo.git".to_owned(),
            cube_repo_id: Some("foo".to_owned()),
            cube_lease_id: Some(lease_id.to_owned()),
            cube_workspace_id: Some(workspace_id.to_owned()),
            workspace_path: Some(format!("/tmp/{workspace_id}")),
            priority: 0,
            preferred_workspace_id: None,
            created_at: "2026-05-07T00:00:00Z".to_owned(),
            started_at: Some("2026-05-07T00:00:00Z".to_owned()),
            finished_at: None,
            pre_start_failure_count: 0,
            dispatch_not_before: None,
        }
    }

    fn workspace(
        workspace_id: &str,
        state: &str,
        lease_id: Option<&str>,
        expires_at: Option<i64>,
    ) -> CubeWorkspaceStatus {
        CubeWorkspaceStatus {
            workspace_id: workspace_id.to_owned(),
            workspace_path: PathBuf::from(format!("/tmp/{workspace_id}")),
            state: state.to_owned(),
            lease_id: lease_id.map(str::to_owned),
            holder: Some("user@host:1234".to_owned()),
            task: Some("test".to_owned()),
            leased_at_epoch_s: Some(1_700_000_000),
            lease_expires_at_epoch_s: expires_at,
        }
    }

    #[tokio::test]
    async fn empty_in_flight_returns_empty_report() {
        let cube = StubCube::ok(Vec::new());
        let report = probe_in_flight_runs(&cube, &[], 1_700_000_000).await;
        assert!(report.verdicts.is_empty());
        assert_eq!(report.live_count, 0);
        assert_eq!(report.dead_count, 0);
        assert_eq!(report.unknown_count, 0);
    }

    #[tokio::test]
    async fn matching_lease_state_marks_run_live() {
        // The cardinal "engine restart, worker still up" case. Cube
        // says workspace W is leased to lease L; persisted state says
        // execution X recorded lease L on workspace W. Verdict: Live.
        // Without this, the dispatcher would re-spawn on top of the
        // running worker.
        let cube = StubCube::ok(vec![workspace(
            "mono-agent-001",
            "leased",
            Some("lease-L"),
            Some(1_700_001_800),
        )]);
        let report = probe_in_flight_runs(
            &cube,
            &[execution("exec-X", "lease-L", "mono-agent-001")],
            1_700_000_000,
        )
        .await;
        assert_eq!(
            report.verdicts.get("exec-X").copied(),
            Some(RunReconcileVerdict::Live),
        );
        assert_eq!(report.live_count, 1);
        assert_eq!(report.dead_count, 0);
        assert_eq!(report.unknown_count, 0);
    }

    #[tokio::test]
    async fn workspace_now_free_marks_run_dead() {
        // Cube has already released the lease (worker exited cleanly
        // before the engine recorded the finish, force-release ran, …).
        // The persisted execution is stale; redispatch is safe.
        let cube = StubCube::ok(vec![workspace("mono-agent-002", "free", None, None)]);
        let report = probe_in_flight_runs(
            &cube,
            &[execution("exec-Y", "lease-L", "mono-agent-002")],
            1_700_000_000,
        )
        .await;
        assert_eq!(
            report.verdicts.get("exec-Y").copied(),
            Some(RunReconcileVerdict::Dead),
        );
        assert_eq!(report.dead_count, 1);
    }

    #[tokio::test]
    async fn lease_id_mismatch_marks_run_dead() {
        // Same workspace, different lease — a second worker has since
        // taken the slot. Don't force-release; just declare ours dead.
        let cube = StubCube::ok(vec![workspace(
            "mono-agent-003",
            "leased",
            Some("lease-NEW"),
            Some(1_700_001_800),
        )]);
        let report = probe_in_flight_runs(
            &cube,
            &[execution("exec-Z", "lease-OLD", "mono-agent-003")],
            1_700_000_000,
        )
        .await;
        assert_eq!(
            report.verdicts.get("exec-Z").copied(),
            Some(RunReconcileVerdict::Dead),
        );
        assert_eq!(report.dead_count, 1);
    }

    #[tokio::test]
    async fn expired_lease_marks_run_dead_even_if_cube_has_not_reaped() {
        // Cube assigns leases a TTL and surfaces it via
        // `lease_expires_at_epoch_s`, but the janitor doesn't run at
        // list time, so an expired lease can still appear `leased`.
        // Treat that as Dead — the worker has been silent long enough
        // that auto-recovery is safer than waiting it out.
        let cube = StubCube::ok(vec![workspace(
            "mono-agent-004",
            "leased",
            Some("lease-L"),
            Some(1_699_999_000),
        )]);
        let report = probe_in_flight_runs(
            &cube,
            &[execution("exec-W", "lease-L", "mono-agent-004")],
            1_700_000_000,
        )
        .await;
        assert_eq!(
            report.verdicts.get("exec-W").copied(),
            Some(RunReconcileVerdict::Dead),
        );
    }

    #[tokio::test]
    async fn workspace_unknown_to_cube_marks_run_unknown() {
        // The persisted workspace id isn't in cube's snapshot at all.
        // Could be a deleted workspace, a race, or a bug; we don't
        // redispatch automatically because we have no signal either
        // way. The operator must resolve.
        let cube = StubCube::ok(Vec::new());
        let report = probe_in_flight_runs(
            &cube,
            &[execution("exec-V", "lease-L", "mono-agent-005")],
            1_700_000_000,
        )
        .await;
        assert_eq!(
            report.verdicts.get("exec-V").copied(),
            Some(RunReconcileVerdict::Unknown),
        );
        assert_eq!(report.unknown_count, 1);
    }

    #[tokio::test]
    async fn cube_list_failure_marks_every_run_unknown() {
        // The brief is explicit: "Don't depend on the events socket
        // being healthy for the probe — that socket may itself be
        // broken." The same caution applies to cube: if we can't talk
        // to it, we MUST NOT default to redispatch (that's the very
        // bug we're fixing). Surface the loud warning and leave every
        // row alone.
        let cube = StubCube::err("simulated cube outage");
        let report = probe_in_flight_runs(
            &cube,
            &[
                execution("exec-A", "lease-A", "mono-agent-001"),
                execution("exec-B", "lease-B", "mono-agent-002"),
            ],
            1_700_000_000,
        )
        .await;
        assert_eq!(
            report.verdicts.get("exec-A").copied(),
            Some(RunReconcileVerdict::Unknown),
        );
        assert_eq!(
            report.verdicts.get("exec-B").copied(),
            Some(RunReconcileVerdict::Unknown),
        );
        assert_eq!(report.unknown_count, 2);
        assert_eq!(report.live_count, 0);
        assert_eq!(report.dead_count, 0);
    }

    #[tokio::test]
    async fn execution_missing_workspace_id_is_unknown() {
        // A row with a cube_lease_id but no cube_workspace_id is too
        // sparse to probe (nothing to match against the snapshot). We
        // shouldn't crash and shouldn't blindly redispatch; mark
        // Unknown and surface the gap.
        let cube = StubCube::ok(vec![workspace(
            "mono-agent-001",
            "leased",
            Some("lease-L"),
            None,
        )]);
        let mut sparse = execution("exec-S", "lease-L", "mono-agent-001");
        sparse.cube_workspace_id = None;
        let report = probe_in_flight_runs(&cube, &[sparse], 1_700_000_000).await;
        assert_eq!(
            report.verdicts.get("exec-S").copied(),
            Some(RunReconcileVerdict::Unknown),
        );
    }

    #[tokio::test]
    async fn mixed_live_dead_unknown_yields_correct_per_row_verdict() {
        // The acceptance test from the work-item brief: "exercise the
        // reconcile path with a mix of live + dead persisted runs and
        // assert the right outcome for each." We add an Unknown row
        // for completeness — real fleets will hit all three.
        let cube = StubCube::ok(vec![
            workspace("mono-agent-001", "leased", Some("lease-A"), Some(1_700_001_800)),
            workspace("mono-agent-002", "free", None, None),
            // mono-agent-003 deliberately omitted → Unknown.
        ]);
        let executions = vec![
            execution("exec-live", "lease-A", "mono-agent-001"),
            execution("exec-dead", "lease-stale", "mono-agent-002"),
            execution("exec-unknown", "lease-Z", "mono-agent-003"),
        ];
        let report = probe_in_flight_runs(&cube, &executions, 1_700_000_000).await;
        assert_eq!(
            report.verdicts.get("exec-live").copied(),
            Some(RunReconcileVerdict::Live),
        );
        assert_eq!(
            report.verdicts.get("exec-dead").copied(),
            Some(RunReconcileVerdict::Dead),
        );
        assert_eq!(
            report.verdicts.get("exec-unknown").copied(),
            Some(RunReconcileVerdict::Unknown),
        );
        assert_eq!(report.live_count, 1);
        assert_eq!(report.dead_count, 1);
        assert_eq!(report.unknown_count, 1);

        // The skip-dispatch helper hides Dead rows so the existing
        // reconcile path's `is_live` predicate flips false for them
        // and abandons their execution — exactly the behaviour the
        // pre-existing tests exercise via `|_| false`.
        let skipped: std::collections::HashSet<&str> = report.skip_dispatch_ids().collect();
        assert!(skipped.contains("exec-live"));
        assert!(skipped.contains("exec-unknown"));
        assert!(!skipped.contains("exec-dead"));
    }
}
