//! Integration tests for the four control verbs added by chore
//! `Implement stubbed bossctl verbs and fix agents stop BossOnly
//! rejection`. Each verb gets a thin end-to-end test through the
//! engine's frontend socket so that re-stubbing them shows up as a
//! red test instead of silently degrading the coordinator.
//!
//! - `cancel_execution` (work cancel): mark a non-terminal execution
//!   `cancelled`; refuse to cancel a row that's already terminal.
//! - `tail_run_transcript` (agents transcript): return the last N
//!   lines of a run's transcript, or surface a structured error when
//!   no transcript path is recorded yet.
//! - `workspace_pool_summary` (workspace summary): proxy whatever the
//!   cube layer returns, plus engine-side annotations. The engine's
//!   in-process cube client is a no-op stub here, so we mainly check
//!   the wire shape and that the response decodes.
//! - `stop_run` (agents stop): regression test for the auth tier on
//!   the StopRun verb. `bossctl agents stop` is the coordinator's
//!   imperative kill switch; humans run it from the Boss pane, the
//!   app shell, *and* from inside worker (slot) panes. The earlier
//!   BossOnly tier rejected the worker-pane case; the verb now uses
//!   AppOrBoss, which accepts worker descendants too.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::{BossClient, wait_for_socket};
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_engine::work::WorkDb;
use boss_protocol::{
    CreateChoreInput, CreateProductInput, CreateRunInput, FrontendEvent, FrontendRequest,
    RequestExecutionInput, WorkItem, WorkItemPatch,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

struct TestEngine {
    socket_path: PathBuf,
    db_path: PathBuf,
    _temp: tempfile::TempDir,
    join: tokio::task::JoinHandle<Result<()>>,
}

impl TestEngine {
    async fn spawn() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let socket_path = temp.path().join("engine.sock");
        let db_path = temp.path().join("state.db");
        let work_config = WorkConfig {
            cwd: temp.path().to_path_buf(),
            db_path: db_path.clone(),
            worker_pool_size: 1,
        };
        let cfg = Arc::new(RuntimeConfig::from_parts(work_config, None));

        let socket_for_serve = socket_path.clone();
        let join = tokio::spawn(async move { serve(cfg, socket_for_serve, None, None).await });

        if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
            return Err(anyhow!(
                "engine never bound socket {}",
                socket_path.display()
            ));
        }

        Ok(Self {
            socket_path,
            db_path,
            _temp: temp,
            join,
        })
    }

    fn socket_str(&self) -> &str {
        self.socket_path.to_str().expect("socket path is utf-8")
    }
}

impl Drop for TestEngine {
    fn drop(&mut self) {
        self.join.abort();
    }
}

/// Returned by `seed_execution` so the test can verify both
/// execution-row state (status flip) and work-item state (kanban
/// column) in the same round-trip.
struct SeededExecution {
    work_item_id: String,
    execution_id: String,
}

/// Create a product + chore + ready execution and return both the
/// chore id and the execution id. Workers don't run in these tests;
/// we just want a row in `work_executions` we can then cancel /
/// inspect, plus the backing work item for kanban-status assertions.
async fn seed_execution(client: &mut BossClient) -> Result<SeededExecution> {
    let product = match client
        .send_request(&FrontendRequest::CreateProduct {
            input: CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@example.com:boss.git".to_owned()),
            },
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: boss_protocol::WorkItem::Product(p),
        } => p,
        other => return Err(anyhow!("unexpected response to CreateProduct: {other:?}")),
    };

    let chore = match client
        .send_request(&FrontendRequest::CreateChore {
            input: CreateChoreInput {
                product_id: product.id.clone(),
                name: "test chore".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
            },
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: boss_protocol::WorkItem::Chore(t),
        }
        | FrontendEvent::WorkItemCreated {
            item: boss_protocol::WorkItem::Task(t),
        } => t,
        other => return Err(anyhow!("unexpected response to CreateChore: {other:?}")),
    };

    let execution = match client
        .send_request(&FrontendRequest::RequestExecution {
            input: RequestExecutionInput {
                work_item_id: chore.id.clone(),
                priority: None,
                preferred_workspace_id: None,
                force: false,
            },
        })
        .await?
    {
        FrontendEvent::ExecutionRequested { execution }
        | FrontendEvent::ExecutionResult { execution }
        | FrontendEvent::ExecutionCreated { execution } => execution,
        other => {
            return Err(anyhow!(
                "unexpected response to RequestExecution: {other:?}"
            ))
        }
    };
    Ok(SeededExecution {
        work_item_id: chore.id,
        execution_id: execution.id,
    })
}

async fn fetch_task_status(client: &mut BossClient, work_item_id: &str) -> Result<String> {
    let response = client
        .send_request(&FrontendRequest::GetWorkItem {
            id: work_item_id.to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::WorkItemResult {
            item: WorkItem::Chore(t),
        }
        | FrontendEvent::WorkItemResult {
            item: WorkItem::Task(t),
        } => Ok(t.status),
        other => Err(anyhow!("unexpected GetWorkItem response: {other:?}")),
    }
}

#[tokio::test]
async fn work_cancel_marks_execution_cancelled() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let SeededExecution {
        work_item_id,
        execution_id,
    } = seed_execution(&mut client).await?;

    // Drive the chore into the Doing column the same way real workers
    // do — manual `active` status flip — so we can verify cancel
    // resets the kanban state. The seed leaves it `todo`.
    client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: work_item_id.clone(),
            patch: WorkItemPatch {
                status: Some("active".to_owned()),
                ..WorkItemPatch::default()
            },
        })
        .await?;
    assert_eq!(fetch_task_status(&mut client, &work_item_id).await?, "active");

    let response = client
        .send_request(&FrontendRequest::CancelExecution {
            execution_id: execution_id.clone(),
        })
        .await?;
    let cancelled = match response {
        FrontendEvent::ExecutionCancelled { execution } => execution,
        other => return Err(anyhow!("unexpected response: {other:?}")),
    };
    assert_eq!(cancelled.id, execution_id);
    assert_eq!(cancelled.status, "cancelled");
    assert!(cancelled.finished_at.is_some(), "cancel must stamp finished_at");

    // Active → todo: the kanban card returns to To-Do because the
    // execution backing it is gone.
    assert_eq!(fetch_task_status(&mut client, &work_item_id).await?, "todo");

    // Cancelling a row that's already terminal should error rather than
    // silently no-op — this is what guards the engine against double
    // cancels racing the reconciler.
    let response = client
        .send_request(&FrontendRequest::CancelExecution { execution_id })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("cancelled") || message.contains("terminal"),
                "expected terminal-status error, got: {message}"
            );
        }
        other => return Err(anyhow!("expected WorkError, got: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn work_cancel_unknown_execution_returns_clear_error() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let response = client
        .send_request(&FrontendRequest::CancelExecution {
            execution_id: "exec_does_not_exist".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("unknown execution"),
                "expected unknown-execution message, got: {message}"
            );
        }
        other => return Err(anyhow!("expected WorkError, got: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn agents_transcript_returns_tail_lines() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let SeededExecution { execution_id, .. } = seed_execution(&mut client).await?;

    // The Run record is the carrier for `transcript_path`; create one
    // directly via WorkDb (no real worker available in this test) and
    // write a small transcript file to disk. Production wires this up
    // through the spawn flow; the engine-side tail behaviour is what
    // we're checking here.
    let transcript_dir = tempfile::tempdir()?;
    let transcript_path = transcript_dir.path().join("transcript.jsonl");
    std::fs::write(
        &transcript_path,
        "{\"event\":\"first\"}\n{\"event\":\"second\"}\n{\"event\":\"third\"}\n",
    )?;
    let work_db = WorkDb::open(engine.db_path.clone())?;
    let run = work_db.create_run(CreateRunInput {
        execution_id,
        agent_id: "test-agent".to_owned(),
        status: Some("active".to_owned()),
        transcript_path: Some(transcript_path.display().to_string()),
        artifacts_path: None,
        result_summary: None,
        error_text: None,
        started_at: None,
        finished_at: None,
    })?;

    let response = client
        .send_request(&FrontendRequest::TailRunTranscript {
            run_id: run.id.clone(),
            lines: 2,
        })
        .await?;
    match response {
        FrontendEvent::RunTranscriptTail {
            run_id,
            transcript_path: returned_path,
            lines,
            truncated,
        } => {
            assert_eq!(run_id, run.id);
            assert_eq!(returned_path, transcript_path.display().to_string());
            assert_eq!(
                lines,
                vec!["{\"event\":\"second\"}".to_owned(), "{\"event\":\"third\"}".to_owned()]
            );
            assert!(truncated, "asking for 2 of 3 lines must mark truncated");
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }

    // Asking for more lines than the file holds returns the whole
    // file and clears the truncated flag.
    let response = client
        .send_request(&FrontendRequest::TailRunTranscript {
            run_id: run.id,
            lines: 10,
        })
        .await?;
    match response {
        FrontendEvent::RunTranscriptTail {
            lines, truncated, ..
        } => {
            assert_eq!(lines.len(), 3);
            assert!(!truncated);
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn agents_transcript_errors_when_run_has_no_transcript_path() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let SeededExecution { execution_id, .. } = seed_execution(&mut client).await?;

    let work_db = WorkDb::open(engine.db_path.clone())?;
    let run = work_db.create_run(CreateRunInput {
        execution_id,
        agent_id: "test-agent".to_owned(),
        status: Some("active".to_owned()),
        transcript_path: None,
        artifacts_path: None,
        result_summary: None,
        error_text: None,
        started_at: None,
        finished_at: None,
    })?;

    let response = client
        .send_request(&FrontendRequest::TailRunTranscript {
            run_id: run.id,
            lines: 5,
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("transcript"),
                "expected transcript-error message, got: {message}"
            );
        }
        other => return Err(anyhow!("expected WorkError, got: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn workspace_summary_returns_pool_snapshot() -> Result<()> {
    // The in-process engine builds a `CommandCubeClient` which would
    // shell out to a real `cube` binary. That isn't available in
    // sandboxed test environments, so this test asserts the verb
    // round-trips at the protocol level: it either returns a
    // (possibly empty) workspace list, or surfaces a structured
    // WorkError from the cube CLI failure. Both prove the verb is
    // wired through the engine — what we're really guarding against
    // is the verb regressing back to the literal `not_implemented`
    // stub it used to return.
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let response = client
        .send_request(&FrontendRequest::WorkspacePoolSummary)
        .await?;
    match response {
        FrontendEvent::WorkspacePoolSummaryResult { .. } => {}
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("cube") || message.contains("workspace"),
                "expected cube-related error, got: {message}"
            );
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn agents_stop_does_not_reject_local_caller_as_boss_only() -> Result<()> {
    // Reproduces the bug from the work item: even after the earlier
    // BossOnly fallback fix, `bossctl agents stop` still hit
    // "stop_run is BossOnly" when invoked from inside a worker
    // (slot) pane — the BossOnly gate explicitly excludes callers
    // that descend from a registered worker shell pid. The verb is
    // now AppOrBoss, which accepts worker descendants too. In the
    // in-process test harness app_pid and boss_pid are both unset
    // (treated as permissive), so any local caller must succeed
    // here; the worker-descendant case is locked in by the macOS
    // unit test `app_or_boss_admits_worker_descendant`.
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::StopRun {
            run_id: "run-does-not-exist".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::RunStopped { .. } => {}
        FrontendEvent::Error { message, .. } => {
            assert!(
                !message.contains("BossOnly")
                    && !message.contains("requires app or Boss authority"),
                "stop_run must not reject local callers on auth grounds: {message}"
            );
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn probe_run_does_not_reject_local_caller_as_boss_only() -> Result<()> {
    // Same regression class as `agents_stop` (PR #218): the BossOnly
    // gate rejected `bossctl probe` calls from worker-pane shells
    // because the gate explicitly excludes descendants of any
    // registered worker pid. The verb is now AppOrBoss — worker
    // descendants are admitted (workers are siblings under the app).
    // The macOS unit test `app_or_boss_admits_worker_descendant`
    // locks in the worker-descendant admission; this test is the
    // wire-shape smoke that asserts probe is reachable at all.
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::ProbeRun {
            run_id: "run-does-not-exist".to_owned(),
            text: "ping".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::ProbeQueued { .. } => {}
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            assert!(
                !message.contains("BossOnly")
                    && !message.contains("requires app or Boss authority"),
                "probe_run must not reject local callers on auth grounds: {message}"
            );
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn agents_send_does_not_reject_local_caller_as_boss_only() -> Result<()> {
    // `bossctl agents send` writes user-typed input into a sibling
    // worker pane. Same auth class as `agents focus` / `probe` /
    // `agents stop` (AppOrBoss). With no run seeded, the verb should
    // pass auth and then fail the run-id lookup with a `WorkError`.
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::SendInputToWorker {
            run_id: "run-does-not-exist".to_owned(),
            text: "hi\n".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::WorkError { .. } => {}
        FrontendEvent::Error { message, .. } => {
            assert!(
                !message.contains("BossOnly")
                    && !message.contains("requires app or Boss authority"),
                "send_input_to_worker must not reject local callers on auth grounds: {message}"
            );
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn probe_run_returns_unique_probe_ids() -> Result<()> {
    // Wire-shape smoke: `ProbeRun` must surface the engine-minted
    // `probe_id` so callers can correlate the queued probe with the
    // eventual `ProbeReplied` push (deeper end-to-end coverage of
    // that flow lives in the `dispatch_probe_reply_emits_…` unit
    // test). Two back-to-back probes for the same run must mint
    // distinct ids.
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let first = client
        .send_request(&FrontendRequest::ProbeRun {
            run_id: "run-xyz".to_owned(),
            text: "first".to_owned(),
        })
        .await?;
    let second = client
        .send_request(&FrontendRequest::ProbeRun {
            run_id: "run-xyz".to_owned(),
            text: "second".to_owned(),
        })
        .await?;
    let id_first = match first {
        FrontendEvent::ProbeQueued { probe_id, .. } => probe_id,
        other => return Err(anyhow!("unexpected response to first probe: {other:?}")),
    };
    let id_second = match second {
        FrontendEvent::ProbeQueued { probe_id, .. } => probe_id,
        other => return Err(anyhow!("unexpected response to second probe: {other:?}")),
    };
    assert!(!id_first.is_empty(), "probe_id must be populated");
    assert!(!id_second.is_empty(), "probe_id must be populated");
    assert_ne!(id_first, id_second, "back-to-back probes must mint distinct ids");
    Ok(())
}

#[tokio::test]
async fn agents_transcript_does_not_reject_local_caller_as_boss_only() -> Result<()> {
    // `bossctl agents transcript` shares the BossOnly→AppOrBoss
    // downgrade with `bossctl probe` and `bossctl agents stop`. This
    // smoke test guards against the verb regressing back to BossOnly
    // and silently locking the coordinator out of worker transcripts.
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::TailRunTranscript {
            run_id: "run-does-not-exist".to_owned(),
            lines: 5,
        })
        .await?;
    match response {
        // Auth passed; the verb went on to fail the run lookup
        // (expected — we did not seed a run).
        FrontendEvent::WorkError { .. } => {}
        FrontendEvent::Error { message, .. } => {
            assert!(
                !message.contains("BossOnly")
                    && !message.contains("requires app or Boss authority"),
                "tail_run_transcript must not reject local callers on auth grounds: {message}"
            );
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn agents_interrupt_does_not_reject_local_caller_as_boss_only() -> Result<()> {
    // `bossctl agents interrupt` ships at the same AppOrBoss tier as
    // `agents focus` / `agents stop` — humans run it from the Boss
    // pane, the app shell, *and* from inside worker (slot) panes.
    // This smoke guards against the verb regressing to BossOnly and
    // silently locking the coordinator out of in-flight Esc.
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::InterruptWorkerPane {
            run_id: "run-does-not-exist".to_owned(),
        })
        .await?;
    match response {
        // Auth passed; the verb went on to fail the run lookup
        // (expected — we did not seed a run).
        FrontendEvent::WorkError { .. } => {}
        FrontendEvent::Error { message, .. } => {
            assert!(
                !message.contains("BossOnly")
                    && !message.contains("requires app or Boss authority"),
                "interrupt_worker_pane must not reject local callers on auth grounds: {message}"
            );
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn agents_reap_marks_running_execution_orphaned() -> Result<()> {
    // Drive a seeded chore from `ready` → `running` (so it has the
    // workspace columns the orphan path needs to preserve), then send
    // a `ReapRun` and verify:
    //   - the engine returns `RunReaped` with status='orphaned',
    //   - cube workspace columns are preserved on the row,
    //   - a second reap on the now-terminal row errors cleanly.
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let SeededExecution { execution_id, .. } = seed_execution(&mut client).await?;

    // Start an actual run on the execution so the workspace columns
    // are populated. `start_execution_run` requires the row to be
    // `ready` first, which `seed_execution` guarantees.
    let work_db = WorkDb::open(engine.db_path.clone())?;
    work_db.start_execution_run(
        &execution_id,
        "test-agent",
        "mono",
        "lease-REAP",
        "mono-agent-007",
        "/tmp/mono-agent-007",
    )?;

    let response = client
        .send_request(&FrontendRequest::ReapRun {
            run_id: execution_id.clone(),
        })
        .await?;
    let reaped = match response {
        FrontendEvent::RunReaped { run_id, execution } => {
            assert_eq!(run_id, execution_id);
            execution
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    };
    assert_eq!(reaped.id, execution_id);
    assert_eq!(reaped.status, "orphaned");
    assert!(reaped.finished_at.is_some(), "reap must stamp finished_at");
    // Workspace columns preserved — that's the whole contract.
    assert_eq!(reaped.cube_lease_id.as_deref(), Some("lease-REAP"));
    assert_eq!(reaped.cube_workspace_id.as_deref(), Some("mono-agent-007"));
    assert_eq!(reaped.workspace_path.as_deref(), Some("/tmp/mono-agent-007"));

    // Second reap on the now-terminal row must error rather than
    // silently no-op — same contract as `cancel_execution`.
    let response = client
        .send_request(&FrontendRequest::ReapRun {
            run_id: execution_id,
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("terminal"),
                "expected terminal-status error, got: {message}"
            );
        }
        other => return Err(anyhow!("expected WorkError, got: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn agents_reap_unknown_run_returns_clear_error() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::ReapRun {
            run_id: "exec_does_not_exist".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("unknown execution"),
                "expected unknown-execution message, got: {message}"
            );
        }
        other => return Err(anyhow!("expected WorkError, got: {other:?}")),
    }
    Ok(())
}

/// Regression: dragging an `autostart=false` chore from Todo to
/// Doing in the macOS kanban must dispatch a worker. The earlier
/// failure shape was that `UpdateWorkItem` flipped status to `active`
/// but no execution row appeared — `tasks.autostart=0` made reconcile
/// silently skip the chore at create time and there was no
/// server-side hook on the human transition to seed one. The
/// kanban-drag fix now fires `RequestExecution` from the engine
/// itself when a task/chore moves into `active` via UpdateWorkItem,
/// so the invariant holds regardless of whether the client also fires
/// the RPC.
///
/// Acceptance:
/// - chore created with `autostart=false` has no execution row,
/// - after `UpdateWorkItem` flips status to `active`, the chore has a
///   non-terminal execution backing it.
#[tokio::test]
async fn kanban_drag_to_doing_dispatches_autostart_false_chore() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = match client
        .send_request(&FrontendRequest::CreateProduct {
            input: CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@example.com:boss.git".to_owned()),
            },
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Product(p),
        } => p,
        other => return Err(anyhow!("unexpected response to CreateProduct: {other:?}")),
    };

    let chore = match client
        .send_request(&FrontendRequest::CreateChore {
            input: CreateChoreInput {
                product_id: product.id.clone(),
                name: "Parked chore".to_owned(),
                description: None,
                // The bug scenario: --no-autostart leaves the chore in
                // `todo` with no execution, waiting for an explicit
                // schedule trigger (drag-to-Doing or `bossctl work
                // start`). Without the fix, the drag does not trigger
                // dispatch and the card is "active" with no worker.
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
            },
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Chore(t),
        }
        | FrontendEvent::WorkItemCreated {
            item: WorkItem::Task(t),
        } => t,
        other => return Err(anyhow!("unexpected response to CreateChore: {other:?}")),
    };
    assert_eq!(chore.status, "todo");
    assert!(!chore.autostart);

    // No execution at create time — autostart=false means the
    // reconcile gate (`task_accepts_execution`) skips creation while
    // the chore sits in `todo`.
    let before = list_executions_for(&mut client, &chore.id).await?;
    assert!(
        before.is_empty(),
        "autostart=false chore must not have a creation-time execution; got {before:?}"
    );

    // Drive the kanban drag-to-Doing: `UpdateWorkItem` with `status =
    // active`. The fix makes this fire `RequestExecution` server-side
    // — without it, the chore sat `active` with no execution.
    let updated = match client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: chore.id.clone(),
            patch: WorkItemPatch {
                status: Some("active".to_owned()),
                ..WorkItemPatch::default()
            },
        })
        .await?
    {
        FrontendEvent::WorkItemUpdated { item } => item,
        other => return Err(anyhow!("unexpected response to UpdateWorkItem: {other:?}")),
    };
    match updated {
        WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, "active"),
        other => return Err(anyhow!("unexpected item kind: {other:?}")),
    }

    // After the drag, the chore must have a non-terminal execution.
    let after = list_executions_for(&mut client, &chore.id).await?;
    assert_eq!(
        after.len(),
        1,
        "drag-to-Doing must create exactly one ready execution; got {after:?}"
    );
    let exec = &after[0];
    assert!(
        matches!(
            exec.status.as_str(),
            "ready" | "queued" | "running" | "waiting_human" | "waiting_dependency"
        ),
        "drag-to-Doing execution should be non-terminal; got status={status:?}",
        status = exec.status
    );
    assert_eq!(exec.work_item_id, chore.id);

    Ok(())
}

/// A second drag from `active` → `active` (idempotent client retry,
/// or status patch from a different field landing alongside an
/// already-active card) must not multiply executions. The fix only
/// fires dispatch on a *transition* into `active`, and even then only
/// when there is no existing non-terminal execution.
#[tokio::test]
async fn kanban_drag_to_doing_is_idempotent_on_repeat() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let product = match client
        .send_request(&FrontendRequest::CreateProduct {
            input: CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@example.com:boss.git".to_owned()),
            },
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Product(p),
        } => p,
        other => return Err(anyhow!("unexpected response to CreateProduct: {other:?}")),
    };

    let chore = match client
        .send_request(&FrontendRequest::CreateChore {
            input: CreateChoreInput {
                product_id: product.id.clone(),
                name: "Parked chore".to_owned(),
                description: None,
                autostart: false,
                priority: None,
                created_via: None,
                repo_remote_url: None,
            },
        })
        .await?
    {
        FrontendEvent::WorkItemCreated {
            item: WorkItem::Chore(t),
        }
        | FrontendEvent::WorkItemCreated {
            item: WorkItem::Task(t),
        } => t,
        other => return Err(anyhow!("unexpected response to CreateChore: {other:?}")),
    };

    // First drag: creates exec #1.
    let _ = client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: chore.id.clone(),
            patch: WorkItemPatch {
                status: Some("active".to_owned()),
                ..WorkItemPatch::default()
            },
        })
        .await?;
    let after_first = list_executions_for(&mut client, &chore.id).await?;
    assert_eq!(after_first.len(), 1, "first drag should create exec");
    let first_exec_id = after_first[0].id.clone();

    // Second UpdateWorkItem that re-asserts `active` (e.g., a stray
    // patch from a sibling field). Must not abandon the existing
    // ready exec or insert a duplicate.
    let _ = client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: chore.id.clone(),
            patch: WorkItemPatch {
                status: Some("active".to_owned()),
                ..WorkItemPatch::default()
            },
        })
        .await?;
    let after_second = list_executions_for(&mut client, &chore.id).await?;
    assert_eq!(
        after_second.len(),
        1,
        "no-op active→active must not create a new execution; got {after_second:?}"
    );
    assert_eq!(
        after_second[0].id, first_exec_id,
        "original execution must be preserved",
    );

    Ok(())
}

async fn list_executions_for(
    client: &mut BossClient,
    work_item_id: &str,
) -> Result<Vec<boss_protocol::WorkExecution>> {
    match client
        .send_request(&FrontendRequest::ListExecutions {
            work_item_id: Some(work_item_id.to_owned()),
        })
        .await?
    {
        FrontendEvent::ExecutionsList { executions, .. } => Ok(executions),
        other => Err(anyhow!("unexpected response to ListExecutions: {other:?}")),
    }
}

/// End-to-end smoke for the worker-facing `boss engine conflicts
/// mark-failed` surface (chore #9 of the merge-conflict design's
/// Phase 3): seed a `conflict_resolutions` row, send the RPC, and
/// assert that the engine flips the row to `failed` with the supplied
/// reason. Also covers the "unknown attempt id" arm and the
/// "already-terminal row" idempotency arm.
#[tokio::test]
async fn mark_conflict_resolution_failed_flips_attempt_status() -> Result<()> {
    let engine = TestEngine::spawn().await?;

    // Seed a product → in_review chore → conflict_resolutions row by
    // talking to the engine's own WorkDb. We avoid the RPC surface
    // for the seed because there's no public protocol-level
    // `insert_conflict_resolution`; that's an engine-internal flow.
    let work_db = WorkDb::open(engine.db_path.clone())?;
    let product = work_db.create_product(CreateProductInput {
        name: "P".to_owned(),
        description: None,
        repo_remote_url: Some("git@example.invalid:foo/bar.git".to_owned()),
    })?;
    let chore = work_db.create_chore(CreateChoreInput {
        product_id: product.id.clone(),
        name: "C".to_owned(),
        description: None,
        autostart: false,
        priority: None,
        created_via: None,
        repo_remote_url: None,
    })?;
    work_db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some("https://github.com/foo/bar/pull/42".to_owned()),
            ..WorkItemPatch::default()
        },
    )?;
    work_db.mark_chore_blocked_merge_conflict(&chore.id, "https://github.com/foo/bar/pull/42")?;
    let attempt = work_db
        .insert_conflict_resolution(boss_engine::work::ConflictResolutionInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore.id.clone(),
            pr_url: "https://github.com/foo/bar/pull/42".to_owned(),
            pr_number: 42,
            head_branch: "feature".to_owned(),
            base_branch: "main".to_owned(),
            base_sha_at_trigger: Some("abc123".to_owned()),
            head_sha_before: Some("def456".to_owned()),
        })?
        .expect("insert should succeed on a fresh row");

    // Drive the engine's WorkDb through a fresh connection of the
    // engine binary by talking to its frontend socket — release the
    // direct handle so its lock doesn't clash with the engine's.
    drop(work_db);

    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::MarkConflictResolutionFailed {
            attempt_id: attempt.id.clone(),
            reason: "product_decision_required".to_owned(),
        })
        .await?;
    let flipped = match response {
        FrontendEvent::ConflictResolutionMarkedFailed { attempt } => attempt,
        other => return Err(anyhow!("unexpected response: {other:?}")),
    };
    assert_eq!(flipped.id, attempt.id);
    assert_eq!(flipped.status, "failed");
    assert_eq!(
        flipped.failure_reason.as_deref(),
        Some("product_decision_required"),
    );
    assert!(flipped.finished_at.is_some(), "finished_at must be stamped");

    // Idempotency: a second call on a now-terminal row surfaces a
    // structured error rather than silently no-op'ing.
    let response = client
        .send_request(&FrontendRequest::MarkConflictResolutionFailed {
            attempt_id: attempt.id.clone(),
            reason: "ignored".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("already terminal") || message.contains("unknown"),
                "expected terminal/unknown message, got: {message}"
            );
        }
        other => return Err(anyhow!("expected WorkError, got: {other:?}")),
    }

    // Unknown attempt id: same error surface, distinguishable by the
    // bogus id in the message body.
    let response = client
        .send_request(&FrontendRequest::MarkConflictResolutionFailed {
            attempt_id: "crz_does_not_exist".to_owned(),
            reason: "nope".to_owned(),
        })
        .await?;
    match response {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("crz_does_not_exist"),
                "expected message to name the bogus id, got: {message}"
            );
        }
        other => return Err(anyhow!("expected WorkError, got: {other:?}")),
    }
    Ok(())
}

#[tokio::test]
async fn workspace_summary_does_not_reject_caller_on_auth_grounds() -> Result<()> {
    // Live-coordinator-session repro: `bossctl workspace summary` was
    // failing AppOrBoss when invoked from a shell that descended from
    // neither the app nor the registered Boss session (e.g., a plain
    // terminal). The verb is read-only and proxies a view that any
    // local user can already get from `cube workspace list`, so it's
    // now User-tier. This smoke asserts no auth rejection fires.
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let response = client
        .send_request(&FrontendRequest::WorkspacePoolSummary)
        .await?;
    match response {
        FrontendEvent::WorkspacePoolSummaryResult { .. } => {}
        // The in-process engine builds a CommandCubeClient which
        // shells out; the cube binary may not be on PATH in the
        // sandbox, so a `WorkError` from the cube layer is acceptable.
        // What we're guarding against is an `Error` carrying an auth
        // rejection.
        FrontendEvent::WorkError { .. } => {}
        FrontendEvent::Error { message, .. } => {
            assert!(
                !message.contains("BossOnly")
                    && !message.contains("requires app or Boss authority")
                    && !message.contains("user-tier check"),
                "workspace_pool_summary must not reject callers on auth grounds: {message}"
            );
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}
