//! Periodic cube-lease heartbeat: keeps every live worker's cube
//! workspace lease from TTL-expiring out from under it.
//!
//! ## Why this exists
//!
//! Cube hands the engine a workspace via a *lease* that carries a TTL
//! (cube's default is 1800 s / 30 min). Cube runs a TTL sweep that
//! reclaims any lease whose expiry has passed — it marks the workspace
//! `free` and clears the lease *even if a worker is still alive in it*.
//! Cube exposes `cube workspace heartbeat <lease>` to push the expiry
//! forward, but only the engine knows a worker is still running, so the
//! engine is the only thing that can call it.
//!
//! Before this sweep the engine never heartbeated anything. Any worker
//! that ran longer than the lease TTL (large chores, multi-bazel builds,
//! reviews) had its workspace reclaimed mid-run: cube flipped it to
//! `free` while a live worker kept editing it, cube and the engine
//! desynced, and the pool filled with "phantom-free" workspaces (cube
//! says free, a live worker is actually there). New dispatches landed on
//! a phantom-free workspace, the engine's occupancy guard refused it,
//! and the pool starved. Recovering took a manual reset of ~30
//! workspaces. This sweep is the root-cause fix.
//!
//! ## Algorithm (mirrors [`crate::dead_pid_sweep`])
//!
//! Every [`heartbeat_interval`] (default 300 s — deliberately ≪ the
//! 1800 s TTL, see "TTL ownership" below):
//!
//! 1. Snapshot [`crate::live_worker_state::LiveWorkerStateRegistry`].
//! 2. For each slot with a live `shell_pid` and non-terminal activity
//!    whose execution is non-terminal and has recorded a
//!    `cube_lease_id`:
//!    1. Probe the PID via `kill(pid, 0)` (shared with the dead-PID
//!       sweep). A **dead** PID is *skipped* — we deliberately stop
//!       heartbeating the instant the process is gone, so the lease
//!       expires on its own and cube frees the workspace within ~TTL
//!       (this is what makes "kill the worker → lease frees within
//!       ~TTL" hold). The dead-PID sweep reaps the slot in parallel.
//!    2. Otherwise call
//!       [`CubeClient::heartbeat_lease`](crate::coordinator::CubeClient::heartbeat_lease)
//!       with an explicit TTL, refreshing the expiry to now + TTL.
//!
//! ## TTL ownership (engine-owned, not implicit)
//!
//! The engine owns the heartbeat-interval ↔ TTL relationship explicitly
//! rather than relying on cube's default: it passes
//! [`LEASE_TTL_SECS`] on every heartbeat and ticks every
//! [`DEFAULT_HEARTBEAT_INTERVAL`]. With 300 s ≪ 1800 s the lease is
//! refreshed ~6× per TTL window, so up to ~4 consecutive missed/failed
//! heartbeats (a transiently busy engine, a flaky cube call) are
//! tolerated before any lease is at risk.
//!
//! ## Engine restart
//!
//! The periodic sweep keys off the in-memory live-worker registry,
//! which is *empty* immediately after an engine restart (it is rebuilt
//! as workers re-send hook events). To stop a long-running worker from
//! being stranded in that gap, two complementary mechanisms work together:
//!
//! 1. [`reheartbeat_live_runs`] runs once at startup and pushes every
//!    `Live`-verdict lease forward by a full TTL immediately.
//! 2. Every subsequent pass of [`run_one_pass`] also scans the DB for
//!    non-terminal executions with a recorded lease that are *not yet
//!    present* in the in-memory registry (the "DB-fallback sweep"). This
//!    covers quiet workers (e.g. a long `bazel build`) that emit no hook
//!    events for many minutes — they receive a continuous stream of
//!    heartbeats until they re-register via a hook or their execution
//!    reaches a terminal state in the DB.
//!
//! ## Relationship with `HeartbeatGuard` (coordinator.rs)
//!
//! `coordinator.rs` also contains a `HeartbeatGuard` that was added for
//! the same 2026-05-12 incident. For in-process / blocking runners
//! (e.g. test fakes where `spawn_worker` blocks until the run
//! completes), the guard fires correctly throughout the run. For the
//! production *pane-spawn* path, `spawn_worker` returns immediately
//! after handing the pane off, and the guard is dropped right after —
//! which means it almost never fires a single beat for a pane worker.
//! That is the accurate root-cause framing: the guard existed but was
//! dropped before it could cover the pane worker's lifetime. This
//! module's periodic sweep is the complementary fix that covers the
//! pane-worker gap. Both mechanisms are intentionally left in place:
//! the guard covers blocking runners; this sweep covers pane workers.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use boss_protocol::{WorkExecution, WorkerActivity};

use crate::coordinator::CubeClient;
use crate::dead_pid_sweep::{PidStatus, probe_pid};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::run_reconcile::{RunReconcileReport, RunReconcileVerdict};
use crate::work::WorkDb;

/// Environment variable overriding the heartbeat cadence (seconds).
pub const HEARTBEAT_INTERVAL_SECS_ENV: &str = "BOSS_CUBE_LEASE_HEARTBEAT_INTERVAL_SECS";

/// Default cadence between heartbeat passes. Deliberately far below the
/// [`LEASE_TTL_SECS`] window so several passes refresh every lease
/// before it could expire.
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(300);

/// TTL (seconds) the engine asks cube to set on every heartbeat. Matches
/// cube's own default of 1800 s, but the engine passes it explicitly so
/// the interval-≪-TTL relationship is owned here and survives a change
/// to cube's default. With [`DEFAULT_HEARTBEAT_INTERVAL`] = 300 s this
/// is a 6× margin.
pub const LEASE_TTL_SECS: u64 = 1800;

/// Per-call timeout for a single `cube workspace heartbeat` subprocess
/// invocation. Mirrors [`crate::coordinator::CUBE_LEASE_TIMEOUT`]: the
/// same cube-hang failure mode that prompted timeouts on lease/repo-ensure
/// calls applies here. Without a bound, one hung heartbeat call would
/// stall the entire pass and leave every other live worker un-heartbeated
/// until the subprocess eventually returned (or never did).
pub const HEARTBEAT_CUBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Read the heartbeat interval from [`HEARTBEAT_INTERVAL_SECS_ENV`],
/// falling back to [`DEFAULT_HEARTBEAT_INTERVAL`]. A zero or unparseable
/// value falls back to the default (a zero interval would busy-loop).
pub fn heartbeat_interval() -> Duration {
    std::env::var(HEARTBEAT_INTERVAL_SECS_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_HEARTBEAT_INTERVAL)
}

/// Counts from one heartbeat pass; logged at `info` when activity occurs.
#[derive(Debug, Default, PartialEq, Eq, bon::Builder)]
pub struct HeartbeatSweepOutcome {
    /// Leases successfully refreshed this pass via the live-registry sweep.
    pub heartbeated: usize,
    /// Leases successfully refreshed via the DB-fallback sweep (in-flight
    /// executions not yet present in the live-worker registry, covering the
    /// post-restart gap until each worker re-registers via hook events).
    pub db_fallback_heartbeated: usize,
    /// Heartbeat calls that errored (lease gone, cube unreachable) or timed
    /// out (cube subprocess hung).
    pub failed: usize,
    /// Live slots whose PID was gone — left to expire on purpose.
    pub dead_pid_skipped: usize,
    /// Live slots whose `shell_pid` is not yet reported (≤ 0), and remote
    /// workers whose shell_pid is permanently 0 (they have no local pid).
    pub no_pid_skipped: usize,
    /// Live slots whose execution has not recorded a `cube_lease_id` yet.
    pub no_lease_skipped: usize,
    /// Slots whose execution/activity is already terminal.
    pub terminal_skipped: usize,
}

impl HeartbeatSweepOutcome {
    fn has_activity(&self) -> bool {
        self.heartbeated > 0 || self.db_fallback_heartbeated > 0 || self.failed > 0
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn. The returned handle is detached by the
/// caller (the loop lives for the engine's lifetime).
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    live_states: Arc<LiveWorkerStateRegistry>,
    cube_client: Arc<dyn CubeClient>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            run_one_pass(
                work_db.as_ref(),
                live_states.as_ref(),
                cube_client.as_ref(),
                dispatch_events.as_ref(),
            )
            .await;
            tokio::time::sleep(interval).await;
        }
    })
}

/// Run a single heartbeat pass: refresh the cube lease of every live
/// worker. Returns a summary of what happened.
pub async fn run_one_pass(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    cube_client: &dyn CubeClient,
    dispatch_events: &dyn DispatchEventSink,
) -> HeartbeatSweepOutcome {
    run_one_pass_impl(
        work_db,
        live_states,
        cube_client,
        dispatch_events,
        HEARTBEAT_CUBE_TIMEOUT,
    )
    .await
}

/// Internal implementation that accepts a configurable per-call timeout
/// (exposed so tests can inject a short timeout without waiting 30 s).
async fn run_one_pass_impl(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    cube_client: &dyn CubeClient,
    dispatch_events: &dyn DispatchEventSink,
    heartbeat_timeout: Duration,
) -> HeartbeatSweepOutcome {
    let mut outcome = HeartbeatSweepOutcome::default();
    let mut registered_run_ids: HashSet<String> = HashSet::new();

    for state in live_states.snapshot() {
        // Slot hasn't reported a shell pid yet — we can't probe liveness,
        // and the lease was just created with a full TTL, so there is no
        // urgency. The next pass picks it up once the app reports the pid.
        //
        // Remote workers (shell_pid = 0 by design, since they have no local
        // pid and their leases live on a remote cube) also land here
        // permanently — they are out of scope for this local sweep.
        if state.shell_pid <= 0 {
            outcome.no_pid_skipped += 1;
            registered_run_ids.insert(state.run_id.clone());
            continue;
        }

        // Terminal activity → the completion / teardown path owns lease
        // release; there is nothing to keep alive.
        if is_terminal_activity(state.activity) {
            outcome.terminal_skipped += 1;
            registered_run_ids.insert(state.run_id.clone());
            continue;
        }

        let execution_id = &state.run_id;
        registered_run_ids.insert(execution_id.clone());

        let execution = match work_db.get_execution(execution_id) {
            Ok(execution) => execution,
            Err(err) => {
                // A live slot whose run_id has no execution row: in-process
                // test slots, or a row deleted out from under us. Nothing to
                // heartbeat.
                tracing::debug!(
                    execution_id,
                    ?err,
                    "cube-lease heartbeat: no execution row for live slot; skipping",
                );
                continue;
            }
        };

        // Execution already terminal (completion raced our snapshot). Its
        // lease is being / has been released by the completion path; do not
        // re-extend it.
        if execution.status.is_terminal() {
            outcome.terminal_skipped += 1;
            continue;
        }

        let Some(lease_id) = execution.cube_lease_id.as_deref() else {
            // Live slot whose execution never reached `start_execution_run`
            // (no lease recorded yet). Nothing to heartbeat this pass.
            outcome.no_lease_skipped += 1;
            continue;
        };

        // Liveness gate: only refresh leases held by a process that is
        // actually alive. A dead PID is LEFT to expire — stopping the
        // heartbeat the instant the process is gone is precisely what makes
        // "kill the worker → lease frees within ~TTL" hold. The dead-PID
        // sweep reaps the slot in parallel.
        if matches!(probe_pid(state.shell_pid), PidStatus::Dead) {
            outcome.dead_pid_skipped += 1;
            continue;
        }
        // Alive, alive-but-not-ours (EPERM), or an unexpected probe error:
        // heartbeat. Erring toward refreshing is deliberate — extending a
        // maybe-dead lease costs at most one TTL window, while failing to
        // extend a live one reclaims a working copy out from under an active
        // worker (the incident this whole module fixes).

        heartbeat_one(
            cube_client,
            dispatch_events,
            execution_id,
            lease_id,
            &execution.work_item_id,
            execution.cube_workspace_id.as_deref().unwrap_or(""),
            heartbeat_timeout,
            &mut outcome.heartbeated,
            &mut outcome.failed,
        )
        .await;
    }

    // DB-fallback sweep: heartbeat in-flight executions not yet present in
    // the live-worker registry. This covers the post-restart gap: the
    // registry is empty until each worker re-registers via hook events, so
    // a quiet worker (e.g. a long bazel build emitting no hooks) would get
    // only the one-shot startup beat and then go un-heartbeated. By scanning
    // the DB every pass we continuously cover such workers until they
    // re-register or their execution reaches a terminal state.
    match work_db.list_in_flight_executions() {
        Ok(in_flight) => {
            for execution in in_flight {
                if registered_run_ids.contains(&execution.id) {
                    continue; // already handled by the registry sweep above
                }
                let Some(lease_id) = execution.cube_lease_id.as_deref() else {
                    continue;
                };
                let execution_id = &execution.id;
                let mut succeeded = 0usize;
                let mut failed = 0usize;
                heartbeat_one(
                    cube_client,
                    dispatch_events,
                    execution_id,
                    lease_id,
                    &execution.work_item_id,
                    execution.cube_workspace_id.as_deref().unwrap_or(""),
                    heartbeat_timeout,
                    &mut succeeded,
                    &mut failed,
                )
                .await;
                if succeeded > 0 {
                    outcome.db_fallback_heartbeated += 1;
                    tracing::debug!(
                        execution_id,
                        lease_id,
                        "cube-lease heartbeat: DB-fallback beat (not yet in live registry)",
                    );
                }
                outcome.failed += failed;
            }
        }
        Err(err) => {
            tracing::warn!(
                ?err,
                "cube-lease heartbeat: failed to query in-flight executions for DB-fallback sweep",
            );
        }
    }

    if outcome.has_activity() {
        tracing::info!(
            heartbeated = outcome.heartbeated,
            db_fallback_heartbeated = outcome.db_fallback_heartbeated,
            failed = outcome.failed,
            dead_pid_skipped = outcome.dead_pid_skipped,
            no_lease_skipped = outcome.no_lease_skipped,
            "cube-lease heartbeat: pass complete",
        );
    }

    outcome
}

/// Execute one `cube workspace heartbeat` call with a timeout. Increments
/// either `*succeeded` or `*failed` and emits a dispatch error event on
/// failure or timeout.
async fn heartbeat_one(
    cube_client: &dyn CubeClient,
    dispatch_events: &dyn DispatchEventSink,
    execution_id: &str,
    lease_id: &str,
    work_item_id: &str,
    cube_workspace_id: &str,
    timeout: Duration,
    succeeded: &mut usize,
    failed: &mut usize,
) {
    let result = tokio::time::timeout(timeout, cube_client.heartbeat_lease(lease_id, Some(LEASE_TTL_SECS))).await;
    match result {
        Ok(Ok(())) => {
            *succeeded += 1;
            tracing::debug!(
                execution_id,
                lease_id,
                ttl_secs = LEASE_TTL_SECS,
                "cube-lease heartbeat: refreshed lease",
            );
        }
        Ok(Err(err)) => {
            *failed += 1;
            tracing::warn!(
                execution_id,
                lease_id,
                error = %format!("{err:#}"),
                "cube-lease heartbeat: failed to refresh lease (cube may have reclaimed it; the worker's workspace is at risk)",
            );
            dispatch_events
                .emit(
                    DispatchEvent::new(Stage::CubeLeaseHeartbeat, Outcome::Error, execution_id)
                        .with_work_item(work_item_id)
                        .with_cube_lease(lease_id)
                        .with_error(&err)
                        .with_details(serde_json::json!({
                            "ttl_secs": LEASE_TTL_SECS,
                            "cube_workspace_id": cube_workspace_id,
                        })),
                )
                .await;
        }
        Err(_elapsed) => {
            *failed += 1;
            let err = anyhow::anyhow!(
                "cube workspace heartbeat timed out after {}s (cube subprocess may be hung)",
                timeout.as_secs()
            );
            tracing::warn!(
                execution_id,
                lease_id,
                timeout_secs = timeout.as_secs(),
                "cube-lease heartbeat: heartbeat call timed out; treating as failure so other leases continue",
            );
            dispatch_events
                .emit(
                    DispatchEvent::new(Stage::CubeLeaseHeartbeat, Outcome::Error, execution_id)
                        .with_work_item(work_item_id)
                        .with_cube_lease(lease_id)
                        .with_error(&err)
                        .with_details(serde_json::json!({
                            "ttl_secs": LEASE_TTL_SECS,
                            "cube_workspace_id": cube_workspace_id,
                            "timed_out": true,
                        })),
                )
                .await;
        }
    }
}

/// Re-heartbeat, once at engine startup, the cube lease of every
/// persisted in-flight execution the startup probe classified `Live`.
///
/// The periodic [`run_one_pass`] sweep keys off the in-memory live-worker
/// registry, which is empty immediately after a restart (rebuilt as
/// workers re-send hook events). Without this, a worker that legitimately
/// outlived the engine restart could have its lease lapse in the gap
/// before its next hook re-registers it. We only touch `Live` verdicts —
/// cube confirmed the lease is still bound to our recorded id and not yet
/// expired — so we never extend a lease that already belongs to someone
/// else. Best-effort: failures are logged, never fatal. Returns the
/// number of leases successfully refreshed.
pub async fn reheartbeat_live_runs(
    cube_client: &dyn CubeClient,
    in_flight: &[WorkExecution],
    report: &RunReconcileReport,
) -> usize {
    let mut heartbeated = 0usize;
    for execution in in_flight {
        if !matches!(
            report.verdicts.get(&execution.id).copied(),
            Some(RunReconcileVerdict::Live)
        ) {
            continue;
        }
        let Some(lease_id) = execution.cube_lease_id.as_deref() else {
            continue;
        };
        let result = tokio::time::timeout(
            HEARTBEAT_CUBE_TIMEOUT,
            cube_client.heartbeat_lease(lease_id, Some(LEASE_TTL_SECS)),
        )
        .await;
        match result {
            Ok(Ok(())) => {
                heartbeated += 1;
                tracing::info!(
                    execution_id = %execution.id,
                    lease_id,
                    ttl_secs = LEASE_TTL_SECS,
                    "cube-lease heartbeat: re-adopted live lease at startup",
                );
            }
            Ok(Err(err)) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    lease_id,
                    error = %format!("{err:#}"),
                    "cube-lease heartbeat: failed to re-adopt live lease at startup (best-effort)",
                );
            }
            Err(_elapsed) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    lease_id,
                    timeout_secs = HEARTBEAT_CUBE_TIMEOUT.as_secs(),
                    "cube-lease heartbeat: startup re-adoption timed out (cube subprocess may be hung); skipping this lease",
                );
            }
        }
    }
    heartbeated
}

fn is_terminal_activity(activity: WorkerActivity) -> bool {
    matches!(activity, WorkerActivity::Terminated | WorkerActivity::Errored)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use boss_protocol::{ExecutionKind, ExecutionStatus, RequestExecutionInput, WorkItemBinding};
    use tempfile::TempDir;

    use super::*;
    use crate::coordinator::{
        CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
    };
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::live_worker_state::LiveWorkerStateRegistry;
    use crate::run_reconcile::{RunReconcileReport, RunReconcileVerdict};
    use crate::work::{CreateChoreInput, CreateProductInput, WorkDb};

    // ─── cube stub ────────────────────────────────────────────────────────────

    /// Records every `heartbeat_lease` call and can be told to fail them.
    #[derive(Default)]
    struct RecordingCube {
        heartbeats: Mutex<Vec<(String, Option<u64>)>>,
        fail: AtomicBool,
    }

    impl RecordingCube {
        fn calls(&self) -> Vec<(String, Option<u64>)> {
            self.heartbeats.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CubeClient for RecordingCube {
        async fn ensure_repo(&self, _: &str) -> Result<CubeRepoHandle> {
            unimplemented!()
        }
        async fn lease_workspace(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: bool,
            _: &[&str],
        ) -> Result<CubeWorkspaceLease> {
            unimplemented!()
        }
        async fn create_change(&self, _: &std::path::Path, _: &str) -> Result<CubeChangeHandle> {
            unimplemented!()
        }
        async fn release_workspace(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn workspace_status(&self, _: &std::path::Path) -> Result<CubeWorkspaceStatus> {
            unimplemented!()
        }
        async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()> {
            self.heartbeats.lock().unwrap().push((lease_id.to_owned(), ttl_seconds));
            if self.fail.load(Ordering::SeqCst) {
                return Err(anyhow!("simulated cube heartbeat failure"));
            }
            Ok(())
        }
        async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
            unimplemented!()
        }
        async fn goto_workspace(&self, _: &std::path::Path, _: u64) -> Result<()> {
            unimplemented!()
        }
        async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
            Ok(vec![])
        }
        async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
            Ok(vec![])
        }
    }

    // ─── helpers ─────────────────────────────────────────────────────────────

    fn open_db() -> (TempDir, Arc<WorkDb>) {
        let dir = TempDir::new().unwrap();
        let db = WorkDb::open(dir.path().join("state.db")).unwrap();
        (dir, Arc::new(db))
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

    fn create_chore(db: &WorkDb, product_id: &str) -> String {
        create_named_chore(db, product_id, "test chore")
    }

    fn create_named_chore(db: &WorkDb, product_id: &str, name: &str) -> String {
        db.create_chore(CreateChoreInput::builder().product_id(product_id).name(name).build())
            .unwrap()
            .id
    }

    /// Create a `ready` execution for `work_item_id`.
    fn ready_execution(db: &WorkDb, work_item_id: &str) -> String {
        db.request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
            .unwrap()
            .id
    }

    /// Create a `running` execution that has recorded `lease_id`.
    fn running_execution_with_lease(db: &WorkDb, work_item_id: &str, lease_id: &str) -> String {
        let execution_id = ready_execution(db, work_item_id);
        db.start_execution_run(
            &execution_id,
            "agent-1",
            "repo",
            lease_id,
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();
        execution_id
    }

    fn register_slot(live_states: &LiveWorkerStateRegistry, slot_id: u8, execution_id: &str, shell_pid: i32) {
        live_states.register_spawn(
            slot_id,
            execution_id,
            "claude-opus-4-8",
            shell_pid,
            Some(WorkItemBinding {
                work_item_id: "wi".to_owned(),
                work_item_name: "test".to_owned(),
                execution_id: execution_id.to_owned(),
            }),
        );
    }

    /// A PID guaranteed not to exist: spawn `true`, wait for it to exit,
    /// reuse its released pid. (Same trick the dead-PID sweep tests use.)
    fn dead_pid() -> i32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        let _ = child.wait();
        pid
    }

    fn execution_value(id: &str, lease_id: &str) -> WorkExecution {
        WorkExecution::builder()
            .id(id)
            .work_item_id(format!("wi-{id}"))
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@example.com:foo.git")
            .cube_repo_id("foo")
            .cube_lease_id(lease_id)
            .cube_workspace_id("mono-agent-001")
            .workspace_path("/tmp/mono-agent-001")
            .created_at("2026-06-15T00:00:00Z")
            .started_at("2026-06-15T00:00:00Z")
            .build()
    }

    // ─── tests ───────────────────────────────────────────────────────────────

    /// The core invariant: a live worker's lease is heartbeated with the
    /// engine-owned TTL every pass.
    #[tokio::test]
    async fn live_lease_is_heartbeated() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_chore(&db, &product_id);
        let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-live");

        let live_states = LiveWorkerStateRegistry::new();
        register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

        let cube = RecordingCube::default();
        let sink = RecordingDispatchEventSink::new();
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink).await;

        assert_eq!(outcome.heartbeated, 1, "live lease must be heartbeated");
        assert_eq!(outcome.failed, 0);
        assert_eq!(cube.calls(), vec![("lease-live".to_owned(), Some(LEASE_TTL_SECS))]);
        assert!(sink.events().await.is_empty(), "no event on the success path");
    }

    /// A slot whose PID is gone is NOT heartbeated — the lease is left to
    /// expire so cube frees the workspace within ~TTL after a kill.
    #[tokio::test]
    async fn dead_pid_lease_is_not_heartbeated() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_chore(&db, &product_id);
        let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-dead");

        let live_states = LiveWorkerStateRegistry::new();
        register_slot(&live_states, 1, &execution_id, dead_pid());

        let cube = RecordingCube::default();
        let sink = RecordingDispatchEventSink::new();
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink).await;

        assert_eq!(outcome.heartbeated, 0);
        assert_eq!(outcome.dead_pid_skipped, 1);
        assert!(cube.calls().is_empty(), "dead PID lease must not be heartbeated");
    }

    /// A slot with no reported pid yet is skipped (the lease is freshly
    /// minted with a full TTL).
    #[tokio::test]
    async fn zero_pid_slot_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_chore(&db, &product_id);
        let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-z");

        let live_states = LiveWorkerStateRegistry::new();
        register_slot(&live_states, 1, &execution_id, 0);

        let cube = RecordingCube::default();
        let sink = RecordingDispatchEventSink::new();
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink).await;

        assert_eq!(outcome.no_pid_skipped, 1);
        assert!(cube.calls().is_empty());
    }

    /// A terminal execution's lease is not re-extended (completion owns it).
    #[tokio::test]
    async fn terminal_execution_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_chore(&db, &product_id);
        let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-term");
        db.mark_execution_orphaned(&execution_id, "test orphan").unwrap();

        let live_states = LiveWorkerStateRegistry::new();
        register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

        let cube = RecordingCube::default();
        let sink = RecordingDispatchEventSink::new();
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink).await;

        assert_eq!(outcome.terminal_skipped, 1);
        assert!(cube.calls().is_empty());
    }

    /// A live slot whose execution never recorded a lease is skipped.
    #[tokio::test]
    async fn missing_lease_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_chore(&db, &product_id);
        let execution_id = ready_execution(&db, &work_item_id); // ready, no lease

        let live_states = LiveWorkerStateRegistry::new();
        register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

        let cube = RecordingCube::default();
        let sink = RecordingDispatchEventSink::new();
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink).await;

        assert_eq!(outcome.no_lease_skipped, 1);
        assert!(cube.calls().is_empty());
    }

    /// A heartbeat failure increments `failed` and emits a single
    /// `cube_lease_heartbeat` error event for observability.
    #[tokio::test]
    async fn heartbeat_failure_emits_error_event() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_chore(&db, &product_id);
        let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-fail");

        let live_states = LiveWorkerStateRegistry::new();
        register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

        let cube = RecordingCube::default();
        cube.fail.store(true, Ordering::SeqCst);
        let sink = RecordingDispatchEventSink::new();
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink).await;

        assert_eq!(outcome.failed, 1);
        assert_eq!(outcome.heartbeated, 0);
        let events = sink.events().await;
        assert_eq!(events.len(), 1, "exactly one failure event");
        assert_eq!(events[0].stage, "cube_lease_heartbeat");
        assert_eq!(events[0].outcome, "error");
        assert_eq!(events[0].cube_lease_id.as_deref(), Some("lease-fail"));
    }

    /// Startup re-adoption heartbeats ONLY the `Live`-verdict leases.
    #[tokio::test]
    async fn reheartbeat_only_touches_live_verdicts() {
        let in_flight = vec![
            execution_value("exec-live", "lease-A"),
            execution_value("exec-dead", "lease-B"),
            execution_value("exec-unknown", "lease-C"),
        ];
        let mut report = RunReconcileReport::default();
        report
            .verdicts
            .insert("exec-live".to_owned(), RunReconcileVerdict::Live);
        report
            .verdicts
            .insert("exec-dead".to_owned(), RunReconcileVerdict::Dead);
        report
            .verdicts
            .insert("exec-unknown".to_owned(), RunReconcileVerdict::Unknown);

        let cube = RecordingCube::default();
        let count = reheartbeat_live_runs(&cube, &in_flight, &report).await;

        assert_eq!(count, 1, "only the Live verdict is re-adopted");
        assert_eq!(cube.calls(), vec![("lease-A".to_owned(), Some(LEASE_TTL_SECS))]);
    }

    #[test]
    fn heartbeat_interval_default_and_override() {
        // Default when unset / unparseable / zero.
        assert_eq!(heartbeat_interval(), DEFAULT_HEARTBEAT_INTERVAL);
    }

    // ─── SlowCube: hangs on "lease-slow", succeeds on everything else ─────────

    /// A cube stub whose `heartbeat_lease` never returns for a designated
    /// "hung" lease id, simulating a stuck cube subprocess. All other leases
    /// complete immediately.
    #[derive(Default)]
    struct SlowCube {
        completed: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl CubeClient for SlowCube {
        async fn ensure_repo(&self, _: &str) -> Result<CubeRepoHandle> {
            unimplemented!()
        }
        async fn lease_workspace(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: bool,
            _: &[&str],
        ) -> Result<CubeWorkspaceLease> {
            unimplemented!()
        }
        async fn create_change(&self, _: &std::path::Path, _: &str) -> Result<CubeChangeHandle> {
            unimplemented!()
        }
        async fn release_workspace(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn workspace_status(&self, _: &std::path::Path) -> Result<CubeWorkspaceStatus> {
            unimplemented!()
        }
        async fn heartbeat_lease(&self, lease_id: &str, _ttl: Option<u64>) -> Result<()> {
            if lease_id == "lease-slow" {
                // Never returns — simulates a hung cube subprocess.
                std::future::pending::<()>().await;
                unreachable!()
            }
            self.completed.lock().unwrap().push(lease_id.to_owned());
            Ok(())
        }
        async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
            unimplemented!()
        }
        async fn goto_workspace(&self, _: &std::path::Path, _: u64) -> Result<()> {
            unimplemented!()
        }
        async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
            Ok(vec![])
        }
        async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
            Ok(vec![])
        }
    }

    /// A hung heartbeat for one slot must NOT block heartbeating of the
    /// remaining slots. The timed-out slot increments `failed`; the other
    /// slot is heartbeated successfully.
    #[tokio::test]
    async fn hung_heartbeat_does_not_block_other_slots() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);

        // Two live executions: one with the hung lease, one normal.
        let wi_slow = create_named_chore(&db, &product_id, "slow chore");
        let exec_slow = running_execution_with_lease(&db, &wi_slow, "lease-slow");

        let wi_fast = create_named_chore(&db, &product_id, "fast chore");
        let exec_fast = running_execution_with_lease(&db, &wi_fast, "lease-fast");

        let live_states = LiveWorkerStateRegistry::new();
        register_slot(&live_states, 1, &exec_slow, std::process::id() as i32);
        register_slot(&live_states, 2, &exec_fast, std::process::id() as i32);

        let cube = SlowCube::default();
        let sink = RecordingDispatchEventSink::new();

        // Use a short timeout so the test does not wait 30 s.
        let outcome = run_one_pass_impl(&db, &live_states, &cube, &sink, Duration::from_millis(50)).await;

        assert_eq!(outcome.failed, 1, "the hung slot must count as failed");
        assert_eq!(outcome.heartbeated, 1, "the non-hung slot must succeed");

        let completed = cube.completed.lock().unwrap().clone();
        assert_eq!(
            completed,
            vec!["lease-fast".to_owned()],
            "only the fast lease completes"
        );

        let events = sink.events().await;
        assert_eq!(events.len(), 1, "one timeout error event");
        assert_eq!(events[0].stage, "cube_lease_heartbeat");
        assert_eq!(events[0].outcome, "error");
        assert_eq!(events[0].cube_lease_id.as_deref(), Some("lease-slow"));
    }

    /// In-flight executions not yet in the live registry (post-restart gap)
    /// are heartbeated via the DB-fallback sweep, so quiet workers with no
    /// hook events continue receiving beats after an engine restart.
    #[tokio::test]
    async fn db_fallback_heartbeats_unregistered_in_flight_executions() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_chore(&db, &product_id);
        let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-orphan");

        // Registry is empty (simulates post-restart state before worker re-hooks).
        let live_states = LiveWorkerStateRegistry::new();

        let cube = RecordingCube::default();
        let sink = RecordingDispatchEventSink::new();
        let outcome = run_one_pass_impl(&db, &live_states, &cube, &sink, HEARTBEAT_CUBE_TIMEOUT).await;

        assert_eq!(
            outcome.db_fallback_heartbeated, 1,
            "DB-fallback must cover unregistered execution"
        );
        assert_eq!(outcome.heartbeated, 0);
        assert_eq!(outcome.failed, 0);
        assert_eq!(cube.calls(), vec![("lease-orphan".to_owned(), Some(LEASE_TTL_SECS))],);
        let _ = execution_id; // used to set up DB row
    }
}
