//! Engine-restart reattach for detached remote workers.
//!
//! Remote-execution UX parity (dispatch-stack PR 4). A remote worker is
//! launched detached (`nohup`) and survives the engine restarting — but
//! the reverse events-socket forward that carries its hook stream rides
//! the engine's `ControlMaster` and dies with the old engine process.
//! Without this pass, a worker that was mid-run when the engine restarted
//! keeps running on the remote but its events (and its eventual `Stop` /
//! PR-URL completion) never reach the new engine: the run strands forever.
//!
//! On startup the engine queries [`WorkDb::list_reattachable_remote_runs`]
//! (active runs on a non-local host whose execution is still non-terminal)
//! and, for each, re-opens the host adapter (which re-establishes the
//! `ControlMaster`) and re-establishes that run's reverse forward. Once the
//! forward is back the worker's next hook lands at the dispatcher exactly
//! as a fresh remote run's would — including the lazy live-status slot
//! registration in `dispatch_live_worker_state` — so the surface
//! converges back to "identical to local" without any per-run state having
//! to be reconstructed up front.
//!
//! The orchestration is a free function over the [`HostAdapterProvider`]
//! seam so it is exercised in-process against a stub provider/adapter; the
//! per-run forward mechanics live in
//! [`crate::ssh_spawn::reestablish_events_forward`], unit-tested against a
//! stubbed transport.

use crate::host_adapter::HostAdapterProvider;
use crate::work::WorkDb;

/// Tally from one [`reattach_remote_runs`] pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReattachSummary {
    /// Runs whose reverse events forward was re-established.
    pub reattached: usize,
    /// Runs whose host could not be resolved or whose adapter / forward
    /// re-establishment failed. Logged per-run; the run stays in the DB
    /// and a later pass (or manual `hosts probe`) can retry.
    pub failed: usize,
    /// Runs whose `host_id` no longer exists in the `hosts` table (host
    /// removed since the run started). Counted separately from `failed`
    /// because it is not an error the engine can recover from.
    pub host_missing: usize,
}

impl ReattachSummary {
    /// True when the pass touched at least one detached remote run.
    pub fn had_candidates(&self) -> bool {
        self.reattached + self.failed + self.host_missing > 0
    }
}

/// Re-establish reverse events forwards for every detached remote run the
/// engine knows about. Best-effort and total: a failure on one run is
/// logged and counted, never propagated, so one unreachable host can't
/// block reattach of the others.
pub async fn reattach_remote_runs(
    work_db: &WorkDb,
    provider: &dyn HostAdapterProvider,
    engine_events_socket: &str,
) -> ReattachSummary {
    let mut summary = ReattachSummary::default();

    let candidates = match work_db.list_reattachable_remote_runs() {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(?err, "reattach: failed to list reattachable remote runs");
            return summary;
        }
    };
    if candidates.is_empty() {
        return summary;
    }
    tracing::info!(
        count = candidates.len(),
        "reattach: re-establishing event forwards for detached remote runs",
    );

    for handle in candidates {
        let host = match work_db.get_host(&handle.host_id) {
            Ok(Some(host)) => host,
            Ok(None) => {
                tracing::warn!(
                    run_id = %handle.run_id,
                    execution_id = %handle.execution_id,
                    host_id = %handle.host_id,
                    "reattach: run references a host no longer in the registry; skipping",
                );
                summary.host_missing += 1;
                continue;
            }
            Err(err) => {
                tracing::warn!(
                    run_id = %handle.run_id,
                    host_id = %handle.host_id,
                    ?err,
                    "reattach: host lookup failed; skipping run",
                );
                summary.failed += 1;
                continue;
            }
        };

        let adapter = match provider.adapter_for(&host).await {
            Ok(adapter) => adapter,
            Err(err) => {
                tracing::warn!(
                    run_id = %handle.run_id,
                    execution_id = %handle.execution_id,
                    host_id = %handle.host_id,
                    error = %format!("{err:#}"),
                    "reattach: could not build host adapter; skipping run",
                );
                summary.failed += 1;
                continue;
            }
        };

        // `reattach_events_forward` keys the remote socket on the
        // worker's BOSS_RUN_ID, which is the execution id.
        match adapter
            .reattach_events_forward(&handle.execution_id, engine_events_socket)
            .await
        {
            Ok(true) => {
                tracing::info!(
                    run_id = %handle.run_id,
                    execution_id = %handle.execution_id,
                    host_id = %handle.host_id,
                    remote_pid = ?handle.remote_pid,
                    "reattach: re-established events forward",
                );
                summary.reattached += 1;
            }
            Ok(false) => {
                // The adapter treated this as a no-op (local). A run with
                // host_id != 'local' should never resolve to a local
                // adapter; surface it rather than silently miscounting.
                tracing::warn!(
                    run_id = %handle.run_id,
                    execution_id = %handle.execution_id,
                    host_id = %handle.host_id,
                    "reattach: adapter reported nothing to reattach for a remote run",
                );
                summary.failed += 1;
            }
            Err(err) => {
                tracing::warn!(
                    run_id = %handle.run_id,
                    execution_id = %handle.execution_id,
                    host_id = %handle.host_id,
                    error = %format!("{err:#}"),
                    "reattach: failed to re-establish events forward; run may strand until a later retry",
                );
                summary.failed += 1;
            }
        }
    }

    tracing::info!(
        reattached = summary.reattached,
        failed = summary.failed,
        host_missing = summary.host_missing,
        "reattach: pass complete",
    );
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::{
        CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
    };
    use crate::host_adapter::HostAdapter;
    use crate::host_registry::Host;
    use crate::runner::RunOutcome;
    use crate::work::{CreateChoreInput, CreateProductInput, WorkDb, WorkItem};
    use anyhow::{Result, anyhow, bail};
    use async_trait::async_trait;
    use boss_protocol::{RequestExecutionInput, WorkExecution};
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Records each `reattach_events_forward(run_id)` call and returns a
    /// canned result. Every other `HostAdapter` method is unused by the
    /// reattach path and panics if hit.
    struct RecordingAdapter {
        host_id: String,
        reattached: Mutex<Vec<String>>,
        result: Result<bool, &'static str>,
    }

    #[async_trait]
    impl HostAdapter for RecordingAdapter {
        fn host_id(&self) -> &str {
            &self.host_id
        }
        async fn ensure_repo(&self, _: &str) -> Result<CubeRepoHandle> {
            unimplemented!()
        }
        async fn lease_workspace(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: bool,
            _: Option<u64>,
        ) -> Result<CubeWorkspaceLease> {
            unimplemented!()
        }
        async fn release_workspace(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> Result<()> {
            unimplemented!()
        }
        async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
            unimplemented!()
        }
        async fn create_change(&self, _: &Path, _: &str) -> Result<CubeChangeHandle> {
            unimplemented!()
        }
        async fn workspace_status(&self, _: &Path) -> Result<CubeWorkspaceStatus> {
            unimplemented!()
        }
        async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
            unimplemented!()
        }
        async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
            unimplemented!()
        }
        fn command_repr(&self, _: &[&str]) -> Option<(String, String)> {
            None
        }
        async fn spawn_worker(
            &self,
            _: &str,
            _: &WorkExecution,
            _: &WorkItem,
            _: &Path,
            _: Option<&str>,
        ) -> Result<RunOutcome> {
            unimplemented!()
        }
        async fn reattach_events_forward(&self, run_id: &str, _engine_events_socket: &str) -> Result<bool> {
            self.reattached.lock().unwrap().push(run_id.to_owned());
            match self.result {
                Ok(v) => Ok(v),
                Err(msg) => bail!("{msg}"),
            }
        }
    }

    struct StubProvider {
        adapter: Arc<RecordingAdapter>,
        /// Host id for which `adapter_for` should fail.
        fail_for: Option<String>,
    }

    #[async_trait]
    impl HostAdapterProvider for StubProvider {
        async fn adapter_for(&self, host: &Host) -> Result<Arc<dyn HostAdapter>> {
            if self.fail_for.as_deref() == Some(host.id.as_str()) {
                return Err(anyhow!("control master open failed"));
            }
            Ok(self.adapter.clone() as Arc<dyn HostAdapter>)
        }
    }

    fn open_db() -> (TempDir, Arc<WorkDb>) {
        let dir = TempDir::new().unwrap();
        let db = WorkDb::open(dir.path().join("state.db")).unwrap();
        (dir, Arc::new(db))
    }

    fn create_chore(db: &WorkDb) -> String {
        let product = db
            .create_product(CreateProductInput {
                name: "p".to_owned(),
                description: None,
                repo_remote_url: Some("https://github.com/test/repo".to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            })
            .unwrap()
            .id;
        db.create_chore(CreateChoreInput {
            product_id: product,
            name: "c".to_owned(),
            description: None,
            repo_remote_url: None,
            priority: None,
            effort_level: None,
            model_override: None,
            created_via: None,
            autostart: true,
            force_duplicate: false,
        })
        .unwrap()
        .id
    }

    /// Start a run for `work_item_id` on `host_id`, returning the
    /// execution id. The run lands in `work_runs` with status `active`.
    fn start_run_on_host(db: &WorkDb, work_item_id: &str, host_id: &str) -> String {
        let execution = db
            .request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
            .unwrap();
        db.start_execution_run_on_host(
            &execution.id,
            "worker-1",
            "repo-1",
            "lease-1",
            "ws-1",
            "/tmp/ws-1",
            host_id,
        )
        .unwrap();
        execution.id
    }

    fn recording_provider(host_id: &str, result: Result<bool, &'static str>) -> (Arc<RecordingAdapter>, StubProvider) {
        let adapter = Arc::new(RecordingAdapter {
            host_id: host_id.to_owned(),
            reattached: Mutex::new(Vec::new()),
            result,
        });
        let provider = StubProvider {
            adapter: adapter.clone(),
            fail_for: None,
        };
        (adapter, provider)
    }

    #[tokio::test]
    async fn reattaches_each_active_remote_run() {
        let (_dir, db) = open_db();
        let chore = create_chore(&db);
        // Register the remote host so get_host resolves it.
        db.add_host("zakalwe", "user@zakalwe", 2, &[]).unwrap();
        let exec_a = start_run_on_host(&db, &chore, "zakalwe");

        let (adapter, provider) = recording_provider("zakalwe", Ok(true));
        let summary = reattach_remote_runs(&db, &provider, "/engine.sock").await;

        assert_eq!(summary.reattached, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.host_missing, 0);
        // The forward is keyed on the execution id (the worker's BOSS_RUN_ID).
        assert_eq!(adapter.reattached.lock().unwrap().as_slice(), &[exec_a]);
    }

    #[tokio::test]
    async fn skips_local_runs() {
        let (_dir, db) = open_db();
        let chore = create_chore(&db);
        // A local run must never be reattached.
        start_run_on_host(&db, &chore, "local");

        let (adapter, provider) = recording_provider("zakalwe", Ok(true));
        let summary = reattach_remote_runs(&db, &provider, "/engine.sock").await;

        assert_eq!(summary, ReattachSummary::default());
        assert!(!summary.had_candidates());
        assert!(adapter.reattached.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn counts_failed_forward() {
        let (_dir, db) = open_db();
        let chore = create_chore(&db);
        db.add_host("zakalwe", "user@zakalwe", 2, &[]).unwrap();
        start_run_on_host(&db, &chore, "zakalwe");

        let (_adapter, provider) = recording_provider("zakalwe", Err("forward refused"));
        let summary = reattach_remote_runs(&db, &provider, "/engine.sock").await;

        assert_eq!(summary.reattached, 0);
        assert_eq!(summary.failed, 1);
    }

    #[tokio::test]
    async fn counts_host_missing_when_host_removed() {
        let (_dir, db) = open_db();
        let chore = create_chore(&db);
        // Start the run on a host, then the run row references "ghost"
        // which is not in the hosts table.
        let execution = db
            .request_execution(RequestExecutionInput::builder().work_item_id(chore.clone()).build())
            .unwrap();
        db.start_execution_run_on_host(
            &execution.id,
            "worker-1",
            "repo-1",
            "lease-1",
            "ws-1",
            "/tmp/ws-1",
            "ghost",
        )
        .unwrap();

        let (adapter, provider) = recording_provider("ghost", Ok(true));
        let summary = reattach_remote_runs(&db, &provider, "/engine.sock").await;

        assert_eq!(summary.host_missing, 1);
        assert_eq!(summary.reattached, 0);
        assert!(adapter.reattached.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn counts_failed_when_adapter_build_fails() {
        let (_dir, db) = open_db();
        let chore = create_chore(&db);
        db.add_host("zakalwe", "user@zakalwe", 2, &[]).unwrap();
        start_run_on_host(&db, &chore, "zakalwe");

        let adapter = Arc::new(RecordingAdapter {
            host_id: "zakalwe".to_owned(),
            reattached: Mutex::new(Vec::new()),
            result: Ok(true),
        });
        let provider = StubProvider {
            adapter: adapter.clone(),
            fail_for: Some("zakalwe".to_owned()),
        };
        let summary = reattach_remote_runs(&db, &provider, "/engine.sock").await;

        assert_eq!(summary.failed, 1);
        assert_eq!(summary.reattached, 0);
        assert!(adapter.reattached.lock().unwrap().is_empty());
    }
}
