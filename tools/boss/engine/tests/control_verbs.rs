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
//! - `stop_run` (agents stop): regression test for the BossOnly auth
//!   check — when no Boss pid is registered (the v2 macOS app has
//!   not yet wired `RegisterBossSession`), the engine must still let
//!   trusted callers through without rejecting them as BossOnly.

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
    RequestExecutionInput,
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

/// Create a product + chore + ready execution and return the
/// execution id. Workers don't run in these tests; we just want a row
/// in `work_executions` we can then cancel / inspect.
async fn seed_execution(client: &mut BossClient) -> Result<String> {
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
    Ok(execution.id)
}

#[tokio::test]
async fn work_cancel_marks_execution_cancelled() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let execution_id = seed_execution(&mut client).await?;

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
async fn agents_transcript_returns_tail_lines() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;
    let execution_id = seed_execution(&mut client).await?;

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
    let execution_id = seed_execution(&mut client).await?;

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
    // Reproduces the bug from the work item: the BossOnly gate had
    // `boss_pid.into_iter().collect()` as its trust set, which goes
    // empty whenever the macOS app hasn't sent `RegisterBossSession`
    // yet — and that includes every coordinator-session use case
    // we've tested in the wild. The fix opens up the gate to
    // descendants of the app trust root (excluding registered worker
    // pids); in the in-process test harness, both app_pid and
    // boss_pid are unset, which is treated as permissive — so a
    // local client must still be allowed to call `stop_run` without
    // hitting "stop_run is BossOnly".
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
                !message.contains("BossOnly"),
                "stop_run must not reject local callers as BossOnly: {message}"
            );
        }
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}
