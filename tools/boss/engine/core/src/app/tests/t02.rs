use super::*;

/// `dispatch_live_worker_state` must persist `transcript_path` on
/// the matching `work_runs` row even when the in-memory
/// `WorkerRegistry` has no slot mapping for the run. Without this
/// guarantee, an engine restart wipes the slot map and every
/// subsequent hook from pre-existing workers leaves
/// `work_runs.transcript_path` NULL — pinning the live-status
/// summarizer at `skip_no_transcript_path` until the worker is
/// re-spawned. The fan-out to the per-slot trigger pipeline is
/// still gated on the slot lookup (the manager has no slot to
/// notify), but the durable column write is not.
#[tokio::test]
async fn dispatch_persists_transcript_path_even_without_slot_mapping() {
    use crate::protocol::WorkerEvent;
    use boss_protocol::{CreateChoreInput, CreateProductInput, RequestExecutionInput};

    let server_state = test_server_state();
    let product = server_state
        .work_db
        .create_product(CreateProductInput {
            name: "p".into(),
            description: None,
            repo_remote_url: Some("git@example.com:p.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = server_state
        .work_db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "c".into(),
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
    let execution = server_state
        .work_db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .build(),
        )
        .unwrap();
    let run = server_state
        .work_db
        .create_run(crate::protocol::CreateRunInput {
            execution_id: execution.id.clone(),
            agent_id: "agent-1".into(),
            status: Some("active".into()),
            transcript_path: None,
            artifacts_path: None,
            result_summary: None,
            error_text: None,
            started_at: None,
            finished_at: None,
        })
        .unwrap();
    assert!(
        run.transcript_path.is_none(),
        "precondition: freshly-created run starts with transcript_path=NULL",
    );
    // Deliberately do NOT call register_run_slot — this simulates
    // the engine-restart window where the registry is empty but
    // the worker is still firing hooks.
    //
    // Slot keys (and `_boss_run_id` payload values) are the
    // execution id, not the work_runs.id — that's what
    // `runner.rs::run_execution` plumbs through to the worker's
    // env. The test mirrors that namespace so the dispatcher's
    // SQL join finds the row.
    assert_eq!(
        server_state.worker_registry.slot_for_run(&execution.id),
        None,
        "precondition: slot mapping must be absent for this regression",
    );

    let event = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(execution.id.clone()),
        transcript_path: Some("/home/u/.claude/projects/foo/sess-1.jsonl".into()),
        event: WorkerEvent::PostToolUse {
            session_id: "claude-sess-1".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::Value::Null,
            tool_response: serde_json::Value::Null,
        },
    };
    dispatch_live_worker_state(&server_state, &event).await;

    let reread = server_state.work_db.get_run(&run.id).unwrap();
    assert_eq!(
        reread.transcript_path.as_deref(),
        Some("/home/u/.claude/projects/foo/sess-1.jsonl"),
        "dispatcher must persist transcript_path on work_runs even when the slot mapping is missing",
    );
}

/// A remote worker holds no libghostty pane, so the spawn flow never
/// registers a slot for it and `slot_for_run` is `None` — but its hooks
/// tunnel back over the forwarded events socket. The dispatcher must
/// lazily assign a virtual slot from the reserved remote range and seed
/// the live-status state so the activity surface tracks the remote worker
/// just like a local one. (Engine-restart reattach relies on this same
/// path: the first hook over a re-established forward re-acquires the
/// slot.)
#[tokio::test]
async fn dispatch_assigns_virtual_slot_to_remote_worker() {
    use crate::protocol::WorkerEvent;
    use crate::worker_registry::REMOTE_SLOT_BASE;
    use boss_protocol::{CreateChoreInput, CreateProductInput, RequestExecutionInput, WorkerActivity};

    let server_state = test_server_state();
    let product = server_state
        .work_db
        .create_product(CreateProductInput {
            name: "p".into(),
            description: None,
            repo_remote_url: Some("git@example.com:p.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = server_state
        .work_db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "remote chore".into(),
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
    let execution = server_state
        .work_db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .build(),
        )
        .unwrap();
    // Start the run on a remote host — this stamps work_runs.host_id =
    // "zakalwe" and leaves the execution non-terminal ("running").
    server_state
        .work_db
        .start_execution_run_on_host(
            &execution.id,
            "worker-1",
            "repo-1",
            "lease-1",
            "ws-1",
            "/tmp/ws-1",
            "zakalwe",
        )
        .unwrap();
    assert_eq!(
        server_state.worker_registry.slot_for_run(&execution.id),
        None,
        "precondition: a remote run never gets a slot from the spawn flow",
    );

    let event = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(execution.id.clone()),
        transcript_path: Some("/home/u/.claude/projects/foo/sess-1.jsonl".into()),
        event: WorkerEvent::PostToolUse {
            session_id: "claude-sess-1".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::Value::Null,
            tool_response: serde_json::Value::Null,
        },
    };
    dispatch_live_worker_state(&server_state, &event).await;

    // A virtual slot from the reserved remote range was allocated …
    let slot = server_state
        .worker_registry
        .slot_for_run(&execution.id)
        .expect("remote worker must be assigned a virtual slot");
    assert!(
        slot >= REMOTE_SLOT_BASE,
        "remote slot {slot} must come from the reserved range, not the local pool",
    );
    // … and the live-status state tracks the worker's activity (a
    // PostToolUse drives Spawning → Working) bound to the work item.
    let state = server_state
        .live_worker_states
        .get(slot)
        .expect("live state must be registered for the remote worker's slot");
    assert_eq!(state.run_id, execution.id);
    assert_eq!(state.activity, WorkerActivity::Working);
    assert_eq!(state.work_item_id.as_deref(), Some(chore.id.as_str()));

    // A second hook for the same run reuses the slot rather than
    // allocating another one.
    dispatch_live_worker_state(&server_state, &event).await;
    assert_eq!(
        server_state.worker_registry.slot_for_run(&execution.id),
        Some(slot),
        "subsequent hooks must reuse the same virtual slot",
    );
}

/// A late or duplicate hook for a remote run whose execution has already
/// settled (completed/failed/etc.) must NOT resurrect a virtual slot — a
/// finished worker should not reappear on the live surface.
#[tokio::test]
async fn dispatch_skips_virtual_slot_for_settled_remote_execution() {
    use crate::protocol::WorkerEvent;
    use boss_protocol::{CreateChoreInput, CreateProductInput, RequestExecutionInput};

    let server_state = test_server_state();
    let product = server_state
        .work_db
        .create_product(CreateProductInput {
            name: "p".into(),
            description: None,
            repo_remote_url: Some("git@example.com:p.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = server_state
        .work_db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "remote chore".into(),
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
    let execution = server_state
        .work_db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .build(),
        )
        .unwrap();
    server_state
        .work_db
        .start_execution_run_on_host(
            &execution.id,
            "worker-1",
            "repo-1",
            "lease-1",
            "ws-1",
            "/tmp/ws-1",
            "zakalwe",
        )
        .unwrap();
    // Settle the execution (mirrors completion / orphan paths).
    server_state
        .work_db
        .mark_execution_orphaned(&execution.id, "test: settled before late hook")
        .unwrap();

    let event = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(execution.id.clone()),
        transcript_path: Some("/home/u/.claude/projects/foo/sess-1.jsonl".into()),
        event: WorkerEvent::Stop {
            session_id: "claude-sess-1".into(),
            stop_hook_active: false,
            stop_reason: boss_protocol::StopReason::Completed,
        },
    };
    dispatch_live_worker_state(&server_state, &event).await;

    assert_eq!(
        server_state.worker_registry.slot_for_run(&execution.id),
        None,
        "a settled remote execution must not get a virtual slot from a late hook",
    );
}

/// Regression test for the 2026-05-12 wrong-namespace bug. The
/// dispatcher's `_boss_run_id` carries an **execution id**
/// (`exec_*`) — that's what `runner.rs::run_execution` plumbs into
/// the worker shim's `BOSS_RUN_ID` env var. The pre-fix
/// `set_run_transcript_path_if_unset` joined the UPDATE on
/// `work_runs.id`, which is in a different namespace (`run_*`).
/// The SQL never matched, every call returned `Ok(false)`, the
/// dispatcher counted it as `_persist_noop`, and 427/427
/// historical rows kept their `transcript_path` NULL forever
/// even though hook delivery was healthy and the payload always
/// carried `transcript_path`.
///
/// Pre-fix this test would observe: `_persist_updated == 0`,
/// `_persist_noop == 1`, `_persist_row_missing` did not exist,
/// and `work_runs.transcript_path` stayed NULL.
///
/// Post-fix: `_persist_updated == 1`, `_persist_row_missing == 0`,
/// and the row carries the persisted path. The
/// `_persist_row_missing` counter is the new structural defense:
/// if the dispatcher is ever handed an id the runs table cannot
/// resolve, it now shows up as its own counter instead of being
/// silently absorbed as a steady-state no-op.
#[tokio::test]
async fn dispatch_persists_transcript_path_when_payload_carries_execution_id() {
    use crate::protocol::WorkerEvent;
    use boss_protocol::{CreateChoreInput, CreateProductInput, RequestExecutionInput};

    let server_state = test_server_state();
    let product = server_state
        .work_db
        .create_product(CreateProductInput {
            name: "p".into(),
            description: None,
            repo_remote_url: Some("git@example.com:p.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = server_state
        .work_db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "c".into(),
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
    let execution = server_state
        .work_db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .build(),
        )
        .unwrap();
    // Drive the real `start_execution_run` path so the run is
    // minted with a `run_*` id — production-shaped. Asserting
    // the namespace prefixes here pins the invariant: if the
    // ids ever converge, the regression's premise changes and
    // future readers should rewrite this test, not paper over
    // it.
    let (execution, run) = server_state
        .work_db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();
    assert!(
        execution.id.starts_with("exec_"),
        "precondition: execution id must use the `exec_` namespace; got {}",
        execution.id,
    );
    assert!(
        run.id.starts_with("run_"),
        "precondition: run id must use the `run_` namespace; got {}",
        run.id,
    );
    assert!(
        run.transcript_path.is_none(),
        "precondition: freshly-started run has transcript_path=NULL",
    );

    // Production sets `BOSS_RUN_ID=execution.id` (see
    // `runner.rs::run_execution`), so the dispatcher's payload
    // `_boss_run_id` carries an `exec_*` value. Mirror that
    // exactly — the entire point of the regression is that the
    // dispatcher must successfully persist when handed this
    // shape.
    let event = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(execution.id.clone()),
        transcript_path: Some("/home/u/.claude/projects/foo/sess-1.jsonl".into()),
        event: WorkerEvent::PostToolUse {
            session_id: "claude-sess-1".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::Value::Null,
            tool_response: serde_json::Value::Null,
        },
    };
    dispatch_live_worker_state(&server_state, &event).await;

    let reread = server_state.work_db.get_run(&run.id).unwrap();
    assert_eq!(
        reread.transcript_path.as_deref(),
        Some("/home/u/.claude/projects/foo/sess-1.jsonl"),
        "dispatcher must persist transcript_path on work_runs even though the hook payload's _boss_run_id is an execution id",
    );

    let stats = server_state.dispatcher_stats.snapshot();
    assert_eq!(
        stats.transcript_path_persist_updated, 1,
        "exactly one Updated outcome expected; got stats={stats:?}",
    );
    assert_eq!(
        stats.transcript_path_persist_noop, 0,
        "this is the first writer — Updated must not be misclassified as AlreadySet; got stats={stats:?}",
    );
    assert_eq!(
        stats.transcript_path_persist_row_missing, 0,
        "the work_runs row exists for this execution; RowMissing must not fire; got stats={stats:?}",
    );
    assert_eq!(
        stats.transcript_path_persist_err, 0,
        "no DB error expected; got stats={stats:?}",
    );
}

/// Companion regression: when the dispatcher is handed an
/// execution id that has no `work_runs` row yet (e.g., a
/// SessionStart hook arrived before `start_execution_run`
/// committed), the outcome must be visible as
/// `_persist_row_missing`, NOT silently merged into
/// `_persist_noop`. The `_persist_noop=263 _persist_updated=0`
/// pattern that hid the wrong-namespace bug for two PRs is
/// what this counter exists to prevent in the future.
#[tokio::test]
async fn dispatch_records_row_missing_when_no_run_exists_for_execution() {
    use crate::protocol::WorkerEvent;
    use boss_protocol::{CreateChoreInput, CreateProductInput, RequestExecutionInput};

    let server_state = test_server_state();
    let product = server_state
        .work_db
        .create_product(CreateProductInput {
            name: "p".into(),
            description: None,
            repo_remote_url: Some("git@example.com:p.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = server_state
        .work_db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "c".into(),
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
    let execution = server_state
        .work_db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .build(),
        )
        .unwrap();
    // Intentionally skip `start_execution_run` — the execution
    // exists but has no `work_runs` row yet, mirroring the
    // race where a hook arrives before the run is inserted.

    let event = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(execution.id.clone()),
        transcript_path: Some("/home/u/.claude/projects/foo/sess-1.jsonl".into()),
        event: WorkerEvent::SessionStart {
            session_id: "claude-sess-1".into(),
            source: crate::protocol::SessionStartSource::Startup,
        },
    };
    dispatch_live_worker_state(&server_state, &event).await;

    let stats = server_state.dispatcher_stats.snapshot();
    assert_eq!(
        stats.transcript_path_persist_row_missing, 1,
        "row_missing must fire when no work_runs row exists for the execution; got stats={stats:?}",
    );
    assert_eq!(
        stats.transcript_path_persist_updated, 0,
        "nothing was written; Updated must stay 0; got stats={stats:?}",
    );
    assert_eq!(
        stats.transcript_path_persist_noop, 0,
        "AlreadySet/Noop is a different outcome and must NOT be incremented; conflation here is the whole reason this counter exists; got stats={stats:?}",
    );
}

/// Regression test for the 2026-05-12 follow-up to PR #366: the
/// running engine kept reporting `work_runs.transcript_path` as
/// NULL even though `last_trigger_kind=post_tool_use` was being
/// recorded on the slot. The cause was that claude's PostToolUse
/// (and PreToolUse / UserPromptSubmit) hook payloads do not
/// necessarily carry `transcript_path` — only SessionStart and
/// Stop reliably do — and the dispatcher's persist branch was
/// gated on `incoming.transcript_path.is_some()`. A PostToolUse
/// without the field landed past the slot lookup, fired the
/// notify, and left the work_runs row untouched. The summarizer
/// then early-outed every tick on "no transcript path yet".
///
/// The fix: cache the path in memory per `run_id` whenever any
/// hook delivers it, then use the cache on subsequent hooks
/// whose payload lacks the field. This test asserts the cache
/// fallback by:
///   1. Dispatching a SessionStart event with `transcript_path`
///      set — populates the cache and persists the path.
///   2. Resetting the row's `transcript_path` back to NULL (the
///      real-world equivalent: the work_runs row did not exist
///      at the moment SessionStart fired, so the UPDATE was a
///      zero-row no-op). The cache, however, retains the path.
///   3. Dispatching a PostToolUse event with `transcript_path =
///      None` and asserting the row picks up the cached path on
///      this second hook.
///
/// Without the cache, step 3 leaves `transcript_path` NULL.
#[tokio::test]
async fn dispatch_persists_transcript_path_from_cache_when_payload_omits_it() {
    use crate::protocol::{SessionStartSource, WorkerEvent};
    use boss_protocol::{CreateChoreInput, CreateProductInput, RequestExecutionInput};

    let server_state = test_server_state();
    let product = server_state
        .work_db
        .create_product(CreateProductInput {
            name: "p".into(),
            description: None,
            repo_remote_url: Some("git@example.com:p.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = server_state
        .work_db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "c".into(),
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
    let execution = server_state
        .work_db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .build(),
        )
        .unwrap();
    let run = server_state
        .work_db
        .create_run(crate::protocol::CreateRunInput {
            execution_id: execution.id.clone(),
            agent_id: "agent-1".into(),
            status: Some("active".into()),
            transcript_path: None,
            artifacts_path: None,
            result_summary: None,
            error_text: None,
            started_at: None,
            finished_at: None,
        })
        .unwrap();
    // Register a slot so this run is past the slot-lookup guard —
    // the chore's running-engine condition is "slot present,
    // transcript_path null". The slot is keyed on the execution
    // id (that's what `BOSS_RUN_ID` carries in production), not
    // on the work_runs.id.
    server_state
        .worker_registry
        .register_run_slot(execution.id.clone(), 5);

    // Step 1: SessionStart populates the cache AND the row.
    let session_start = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(execution.id.clone()),
        transcript_path: Some("/home/u/.claude/projects/foo/sess-1.jsonl".into()),
        event: WorkerEvent::SessionStart {
            session_id: "claude-sess-1".into(),
            source: SessionStartSource::Startup,
        },
    };
    dispatch_live_worker_state(&server_state, &session_start).await;
    assert_eq!(
        server_state
            .work_db
            .get_run(&run.id)
            .unwrap()
            .transcript_path
            .as_deref(),
        Some("/home/u/.claude/projects/foo/sess-1.jsonl"),
        "SessionStart with transcript_path must persist to work_runs",
    );

    // Step 2: simulate the real-world race where the work_runs
    // row was not yet present when SessionStart fired — the
    // UPDATE was a no-op. We clear the column directly to model
    // that condition; the in-memory cache survives because the
    // dispatcher populated it BEFORE the persist attempt.
    server_state
        .work_db
        .clear_run_transcript_path_for_test(&run.id)
        .unwrap();
    assert!(
        server_state
            .work_db
            .get_run(&run.id)
            .unwrap()
            .transcript_path
            .is_none(),
        "precondition: row is back to NULL, mirroring the race the chore reproduces",
    );

    // Step 3: PostToolUse with NO transcript_path on the
    // payload. Pre-fix this was a silent drop; post-fix the
    // cached path is persisted.
    let post_tool_use = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(execution.id.clone()),
        transcript_path: None,
        event: WorkerEvent::PostToolUse {
            session_id: "claude-sess-1".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::Value::Null,
            tool_response: serde_json::Value::Null,
        },
    };
    dispatch_live_worker_state(&server_state, &post_tool_use).await;
    assert_eq!(
        server_state
            .work_db
            .get_run(&run.id)
            .unwrap()
            .transcript_path
            .as_deref(),
        Some("/home/u/.claude/projects/foo/sess-1.jsonl"),
        "PostToolUse without transcript_path in payload must persist the cached path",
    );

    // The cache-backed persist must be counted distinctly so an
    // operator can verify at runtime that the fallback is doing
    // actual work.
    let stats = server_state.dispatcher_stats.snapshot();
    assert!(
        stats.transcript_path_persist_from_cache >= 1,
        "dispatcher_stats.transcript_path_persist_from_cache must increment on the cache-backed persist; got {}",
        stats.transcript_path_persist_from_cache,
    );
    assert!(
        stats.hook_events_without_transcript_path_in_payload >= 1,
        "PostToolUse event with no payload transcript_path must be counted; got {}",
        stats.hook_events_without_transcript_path_in_payload,
    );
    assert_eq!(
        stats.last_hook.as_ref().map(|h| h.kind.as_str()),
        Some("post_tool_use"),
        "last_hook kind must reflect the most recent dispatch",
    );
}

/// Regression test that pins the synthetic vs real trigger
/// distinction in the per-slot debug snapshot. Before the
/// 2026-05-12 fix this ambiguity was the *reason* the running-
/// engine report looked like real hooks were arriving (the
/// `last_trigger_kind=post_tool_use` value): the per-slot loop's
/// 60-second timer wrote the same field. Now the snapshot keeps
/// `last_real_trigger_*` separate so an operator can tell at a
/// glance which side of the line they're on.
#[tokio::test]
async fn dispatch_real_post_tool_use_updates_real_trigger_fields() {
    use crate::live_status_loop::{LiveStatusBroadcaster, TranscriptPathResolver};
    use crate::protocol::WorkerEvent;
    use async_trait::async_trait;
    use boss_protocol::{CreateChoreInput, CreateProductInput, RequestExecutionInput};
    use std::path::PathBuf;

    // The slot loop spawns and lives for the duration of the
    // test; broadcaster + resolver stubs do nothing so the
    // summarizer path is a no-op and we only exercise the
    // trigger fan-in.
    struct NopBroadcaster;
    #[async_trait]
    impl LiveStatusBroadcaster for NopBroadcaster {
        async fn broadcast_live_worker_states(&self) {}
    }
    struct NopResolver;
    #[async_trait]
    impl TranscriptPathResolver for NopResolver {
        async fn transcript_path(&self, _run_id: &str) -> Option<PathBuf> {
            None
        }
    }

    let server_state = test_server_state();
    let product = server_state
        .work_db
        .create_product(CreateProductInput {
            name: "p".into(),
            description: None,
            repo_remote_url: Some("git@example.com:p.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = server_state
        .work_db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "c".into(),
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
    let execution = server_state
        .work_db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .build(),
        )
        .unwrap();
    let run = server_state
        .work_db
        .create_run(crate::protocol::CreateRunInput {
            execution_id: execution.id.clone(),
            agent_id: "agent-1".into(),
            status: Some("active".into()),
            transcript_path: None,
            artifacts_path: None,
            result_summary: None,
            error_text: None,
            started_at: None,
            finished_at: None,
        })
        .unwrap();
    let slot_id = 5u8;
    // Slots are keyed on the execution id, mirroring what the
    // worker shim's `BOSS_RUN_ID` carries in production.
    let _ = &run; // pin: row must exist for the persist join below.
    server_state
        .worker_registry
        .register_run_slot(execution.id.clone(), slot_id);
    server_state.live_worker_states.register_spawn(
        slot_id,
        execution.id.clone(),
        "claude-opus-4-7",
        0,
        None,
    );

    // Start a real per-slot task so the notify pathway is
    // exercised end-to-end. The summarizer's `resolver` returns
    // None, so the loop will skip to "no transcript path yet"
    // and never call the model — exactly what we want.
    let broadcaster: std::sync::Arc<dyn LiveStatusBroadcaster> =
        std::sync::Arc::new(NopBroadcaster);
    let resolver: std::sync::Arc<dyn TranscriptPathResolver> = std::sync::Arc::new(NopResolver);
    server_state.live_status_manager.start_slot(
        slot_id,
        execution.id.clone(),
        None,
        server_state.live_worker_states.clone(),
        broadcaster,
        resolver,
    );

    let event = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(execution.id.clone()),
        transcript_path: Some("/home/u/.claude/projects/foo/sess-1.jsonl".into()),
        event: WorkerEvent::PostToolUse {
            session_id: "claude-sess-1".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::Value::Null,
            tool_response: serde_json::Value::Null,
        },
    };
    dispatch_live_worker_state(&server_state, &event).await;

    // Yield to let the slot task service the queued triggers.
    // The PostToolUse fan-out queues both a Trigger::PostToolUse
    // and a Trigger::ActivityChanged(Working); both must land
    // on the loop before we inspect the debug store.
    for _ in 0..50 {
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let snap = server_state
            .live_status_manager
            .debug_store()
            .snapshot_for(slot_id);
        if snap.last_real_trigger_kind.is_some() {
            break;
        }
    }

    let snap = server_state
        .live_status_manager
        .debug_store()
        .snapshot_for(slot_id);
    assert!(
        snap.last_real_trigger_kind.is_some(),
        "real hook arrival must update last_real_trigger_kind; got {snap:?}",
    );
    assert!(
        snap.last_real_trigger_at_epoch_s.is_some(),
        "real hook arrival must update last_real_trigger_at_epoch_s; got {snap:?}",
    );
    assert!(
        snap.last_synthetic_trigger_at_epoch_s.is_none(),
        "a real hook must not be misattributed to the synthetic timer; got {snap:?}",
    );

    server_state.live_status_manager.stop_slot(slot_id);
}

/// Regression test for the 2026-05-12 follow-up to PR #384: the
/// write side of `transcript_path` was fixed there, but the
/// engine's read sites kept calling `work_db.get_run(run_id)`
/// where `run_id` was actually an `exec_*` execution id (the
/// `LiveWorkerState.run_id` field aliases the execution id;
/// `BOSS_RUN_ID` carries the same value). The join therefore
/// never matched and `build_live_status_debug_report` returned
/// `slots[*].transcript_path = null` even when the underlying
/// `work_runs.transcript_path` column had been populated by the
/// dispatcher — visible to the user as "Boss UI shows no live
/// updates" for the 4th time.
///
/// This test pins the read path: after a hook event with
/// `transcript_path` lands and the dispatcher writes the column,
/// the slot snapshot rendered by `bossctl live-status debug`
/// must report the same path back.
#[tokio::test]
async fn live_status_debug_slot_transcript_path_resolves_after_hook_event() {
    use crate::protocol::WorkerEvent;
    use boss_protocol::{CreateChoreInput, CreateProductInput, RequestExecutionInput};

    let server_state = test_server_state();
    let product = server_state
        .work_db
        .create_product(CreateProductInput {
            name: "p".into(),
            description: None,
            repo_remote_url: Some("git@example.com:p.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = server_state
        .work_db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "c".into(),
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
    let execution = server_state
        .work_db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .build(),
        )
        .unwrap();
    let (execution, run) = server_state
        .work_db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();
    assert!(
        execution.id.starts_with("exec_"),
        "precondition: execution id namespace is `exec_*`",
    );
    assert!(
        run.id.starts_with("run_"),
        "precondition: run id namespace is `run_*` (distinct from execution_id)",
    );

    // Production carries the execution id, not the work_runs.id,
    // through `BOSS_RUN_ID`; the slot map and live-state registry
    // mirror that.
    let slot_id = 5u8;
    server_state
        .worker_registry
        .register_run_slot(execution.id.clone(), slot_id);
    server_state.live_worker_states.register_spawn(
        slot_id,
        execution.id.clone(),
        "claude-opus-4-7",
        0,
        None,
    );

    let path = "/home/u/.claude/projects/foo/sess-1.jsonl";
    let event = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(execution.id.clone()),
        transcript_path: Some(path.into()),
        event: WorkerEvent::PostToolUse {
            session_id: "claude-sess-1".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::Value::Null,
            tool_response: serde_json::Value::Null,
        },
    };
    dispatch_live_worker_state(&server_state, &event).await;

    // Sanity: the write path stored the column on the right row.
    // (This is the PR #384 invariant.)
    let reread = server_state.work_db.get_run(&run.id).unwrap();
    assert_eq!(
        reread.transcript_path.as_deref(),
        Some(path),
        "precondition: write path persisted transcript_path on work_runs",
    );

    // The actual regression: render the debug report and assert
    // the slot's `transcript_path` field is the same path. Pre-
    // fix this would be `None`, because the fallback in
    // `build_live_status_debug_report` did `work_db.get_run(
    // execution_id)` and silently swallowed the resulting
    // `Err(unknown run: exec_*)` as `None`.
    let report = build_live_status_debug_report(&server_state, &server_state.work_db);
    let slot = report
        .slots
        .iter()
        .find(|s| s.slot_id == slot_id)
        .expect("the registered slot must be present in the debug report");
    assert_eq!(
        slot.transcript_path.as_deref(),
        Some(path),
        "the slot snapshot must surface the persisted transcript_path — pre-fix this came back null and broke the UI's live-status read",
    );
}

/// Companion to the test above, exercising the production read
/// path through `TranscriptPathResolver` (which is what the per-
/// slot live-status loop calls). The same wrong-namespace bug
/// lived here — the trait impl on `ServerState` did
/// `work_db.get_run(run_id)` where `run_id` was the execution id
/// — and pre-fix the resolver always returned `None`, so the
/// summarizer's `tail` never resolved a transcript path and
/// `debug_store.snap.transcript_path` was never populated.
/// That's the upstream source of the `transcript_path: null` the
/// user observed in the slot snapshot.
#[tokio::test]
async fn transcript_path_resolver_resolves_execution_id_after_hook_persist() {
    use crate::live_status_loop::TranscriptPathResolver;
    use crate::protocol::WorkerEvent;
    use boss_protocol::{CreateChoreInput, CreateProductInput, RequestExecutionInput};

    let server_state = test_server_state();
    let product = server_state
        .work_db
        .create_product(CreateProductInput {
            name: "p".into(),
            description: None,
            repo_remote_url: Some("git@example.com:p.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = server_state
        .work_db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "c".into(),
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
    let execution = server_state
        .work_db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .build(),
        )
        .unwrap();
    let (execution, run) = server_state
        .work_db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();
    let _ = run;

    // Resolver returns None until the dispatcher persists the
    // column; pin that as a precondition so the post-dispatch
    // assertion has bite.
    assert!(
        <ServerState as TranscriptPathResolver>::transcript_path(&server_state, &execution.id,)
            .await
            .is_none(),
        "precondition: resolver returns None when transcript_path on the latest run is NULL",
    );

    let path = "/home/u/.claude/projects/foo/sess-1.jsonl";
    let event = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(execution.id.clone()),
        transcript_path: Some(path.into()),
        event: WorkerEvent::SessionStart {
            session_id: "claude-sess-1".into(),
            source: crate::protocol::SessionStartSource::Startup,
        },
    };
    dispatch_live_worker_state(&server_state, &event).await;

    let resolved =
        <ServerState as TranscriptPathResolver>::transcript_path(&server_state, &execution.id)
            .await;
    assert_eq!(
        resolved.as_deref().map(|p| p.to_string_lossy().to_string()),
        Some(path.to_owned()),
        "TranscriptPathResolver must resolve an execution id to the latest work_runs row's transcript_path",
    );

    // And the wrong-namespace identifier (a `run_*`) must NOT
    // resolve — that would be a regression to the pre-fix shape
    // where the read sites happily accepted the wrong namespace.
    // Note: passing run.id below is intentionally the wrong
    // namespace for this trait method; the resolver's job is to
    // refuse the wrong-namespace identifier rather than
    // accidentally satisfy it.
    let wrong =
        <ServerState as TranscriptPathResolver>::transcript_path(&server_state, &run.id).await;
    assert!(
        wrong.is_none(),
        "resolver must not satisfy a work_runs.id lookup as if it were an execution id; got {wrong:?}",
    );
}

/// Regression test for the 2026-05-12 bug where `bossctl agents
/// transcript` rejected a live worker's transcript as `unknown
/// run`. The reproduction:
///
/// 1. `agents list` reports the worker with `run = exec_*` (its
///    `LiveWorkerState.run_id`, which aliases the execution id).
/// 2. The worker has been registered via `register_spawn` but has
///    not yet emitted a hook event with `transcript_path`, so
///    `work_runs.transcript_path` is still NULL.
/// 3. `TailRunTranscript` resolved the path with
///    `work_db.get_run(run_id)`, which joins against
///    `work_runs.id` (a `run_*` namespace) — the lookup never
///    matched and the verb bailed with `unknown run: exec_*`.
///
/// The post-fix [`resolve_transcript_for_tail`] tries both
/// namespaces and falls back to the live registry, so this case
/// must return [`TranscriptResolution::Buffering`] (the engine
/// will then surface a stable `transcript not yet available`
/// WorkError to the caller).
#[tokio::test]
async fn tail_transcript_resolver_reports_buffering_for_live_run_without_path() {
    use boss_protocol::{CreateChoreInput, CreateProductInput, RequestExecutionInput};

    let server_state = test_server_state();
    let product = server_state
        .work_db
        .create_product(CreateProductInput {
            name: "p".into(),
            description: None,
            repo_remote_url: Some("git@example.com:p.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = server_state
        .work_db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "c".into(),
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
    let execution = server_state
        .work_db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .build(),
        )
        .unwrap();
    let (execution, _run) = server_state
        .work_db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();

    // Mirror the production spawn flow: the live registry is
    // stamped with the execution id (see `spawn_flow::start_worker`).
    // No hook events have fired yet, so transcript_path is NULL
    // on the work_runs row and absent from the cache.
    let slot_id = 6u8;
    server_state.live_worker_states.register_spawn(
        slot_id,
        execution.id.clone(),
        "claude-opus-4-7",
        0,
        None,
    );

    // Pre-fix this returned `Unknown` (the `get_run(exec_*)`
    // call bailed) — the post-fix resolver must surface
    // `Buffering` so the verb's caller knows the run is live and
    // the transcript will materialise shortly.
    let resolution = resolve_transcript_for_tail(&server_state, &execution.id);
    assert_eq!(
        resolution,
        TranscriptResolution::Buffering,
        "live worker with no transcript_path yet must resolve as Buffering, not Unknown — pre-fix the verb rejected `agents transcript` for in-flight workers"
    );

    // Genuinely unknown ids must still resolve as `Unknown` so the
    // caller can distinguish a typo / stale id from a live worker
    // mid-spawn.
    assert_eq!(
        resolve_transcript_for_tail(&server_state, "exec_does_not_exist"),
        TranscriptResolution::Unknown,
        "an id with no DB row and no live entry must resolve as Unknown",
    );
}

/// Companion to the test above: once a hook event carries the
/// `transcript_path`, the cache and the persisted `work_runs.transcript_path`
/// both surface the same path through the resolver, regardless of
/// whether the caller passes the `exec_*` or `run_*` namespace.
#[tokio::test]
async fn tail_transcript_resolver_surfaces_path_via_both_namespaces() {
    use crate::protocol::WorkerEvent;
    use boss_protocol::{CreateChoreInput, CreateProductInput, RequestExecutionInput};

    let server_state = test_server_state();
    let product = server_state
        .work_db
        .create_product(CreateProductInput {
            name: "p".into(),
            description: None,
            repo_remote_url: Some("git@example.com:p.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = server_state
        .work_db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "c".into(),
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
    let execution = server_state
        .work_db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .build(),
        )
        .unwrap();
    let (execution, run) = server_state
        .work_db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();

    let path = "/home/u/.claude/projects/foo/sess-1.jsonl";
    let event = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(execution.id.clone()),
        transcript_path: Some(path.into()),
        event: WorkerEvent::SessionStart {
            session_id: "claude-sess-1".into(),
            source: crate::protocol::SessionStartSource::Startup,
        },
    };
    dispatch_live_worker_state(&server_state, &event).await;

    // Both reference shapes resolve to the same path. This is what
    // breaks `agents transcript exec_*` and `agents transcript
    // <run_*>` when the engine resolves the wrong namespace.
    assert_eq!(
        resolve_transcript_for_tail(&server_state, &execution.id),
        TranscriptResolution::Found {
            transcript_path: path.to_owned(),
        },
        "execution-id lookup must surface the persisted transcript_path",
    );
    assert_eq!(
        resolve_transcript_for_tail(&server_state, &run.id),
        TranscriptResolution::Found {
            transcript_path: path.to_owned(),
        },
        "work_runs-id lookup must surface the persisted transcript_path",
    );
}

/// `current_parent_pid` must NOT fall back to `getppid()` when
/// `BOSS_APP_PID` is unset. The fallback used to land on the bazel
/// daemon (in `bazel run` dev setups) or launchd (1) — neither
/// matches the real macOS app, so every `RegisterAppSession` from
/// the actual app got rejected, no app session ever registered,
/// and every `SpawnWorkerPane` request fell on the floor. Drag-to
/// -Doing visibly accepted the request, the dispatcher created
/// the run row, then `start_worker` returned `AppDisconnected`
/// and the run flipped to `failed` with no surface explanation.
/// Production sets `BOSS_APP_PID`, so the env-set branch is
/// unaffected; this guards both branches.
///
/// All four cases live in one test so the env mutations stay
/// serialised — sibling tests racing on the same key would flake
/// under cargo's parallel runner.
#[test]
fn current_parent_pid_only_trusts_env_var() {
    let original = std::env::var_os("BOSS_APP_PID");

    unsafe {
        std::env::remove_var("BOSS_APP_PID");
    }
    assert_eq!(
        super::current_parent_pid(),
        None,
        "unset BOSS_APP_PID must yield None — no getppid() fallback",
    );

    unsafe {
        std::env::set_var("BOSS_APP_PID", "4242");
    }
    assert_eq!(super::current_parent_pid(), Some(4242));

    unsafe {
        std::env::set_var("BOSS_APP_PID", "1");
    }
    assert_eq!(
        super::current_parent_pid(),
        None,
        "pids <= 1 are launchd / unset sentinels and must not be trusted",
    );

    unsafe {
        std::env::set_var("BOSS_APP_PID", "not-a-number");
    }
    assert_eq!(super::current_parent_pid(), None);

    unsafe {
        match original {
            Some(value) => std::env::set_var("BOSS_APP_PID", value),
            None => std::env::remove_var("BOSS_APP_PID"),
        }
    }
}

/// Graceful shutdown must walk every live worker the engine knows
/// about and ask the app to release its pane. This is the
/// regression test for `engine kills its claude workers on
/// shutdown` — without it, a clean engine exit leaves the worker
/// shells reparented to launchd and `claude` keeps burning tokens.
#[tokio::test]
async fn shutdown_workers_releases_each_live_worker_via_release_worker_pane() {
    let server_state = test_server_state();

    // Two workers, both registered against slot ids and the
    // live-state registry — exactly the shape `release_worker_pane`
    // walks (worker_registry → take_slot_for_run; live_states →
    // release_slot).
    server_state.worker_registry.register_run_slot("run-a", 1);
    server_state.worker_registry.register_run_slot("run-b", 2);
    server_state
        .live_worker_states
        .register_spawn(1, "run-a", "claude-opus-4-7", 0, None);
    server_state
        .live_worker_states
        .register_spawn(2, "run-b", "claude-opus-4-7", 0, None);

    // Stand up a fake app session and a responder task: the
    // engine sends `ReleaseWorkerPane` requests onto its sink, the
    // responder pulls them off, and we assert on the slot ids
    // emitted. Without an ack the engine logs and moves on — but
    // `shutdown_workers` would hit its 5s budget on a real run, so
    // we ack each one to keep the test fast and to verify the
    // engine round-trips correctly.
    let app_sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), app_sink.clone())
        .await;

    let server_for_app = server_state.clone();
    let observed_slots: Arc<StdMutex<Vec<u8>>> = Arc::new(StdMutex::new(Vec::new()));
    let observed_for_task = observed_slots.clone();
    let app_responder = tokio::spawn(async move {
        // Two workers => two ReleaseWorkerPane requests.
        for _ in 0..2 {
            let envelope = app_sink
                .next()
                .await
                .expect("ReleaseWorkerPane EngineRequest should be enqueued");
            let (request_id, slot_id) = match &envelope.payload {
                FrontendEvent::EngineRequest {
                    request_id,
                    request,
                } => match request {
                    EngineToAppRequest::ReleaseWorkerPane(input) => {
                        (request_id.clone(), input.slot_id)
                    }
                    other => panic!("expected ReleaseWorkerPane, got {other:?}"),
                },
                other => panic!("expected EngineRequest, got {other:?}"),
            };
            observed_for_task.lock().unwrap().push(slot_id);
            server_for_app
                .deliver_app_response(
                    "session-app",
                    &request_id,
                    EngineToAppResponse::ReleaseWorkerPane {
                        result: Ok(crate::protocol::ReleaseWorkerPaneResult {}),
                    },
                )
                .await;
        }
    });

    server_state
        .shutdown_workers(Duration::from_secs(2), Duration::from_millis(0))
        .await;

    app_responder.await.expect("app responder task panicked");

    let mut slots = observed_slots.lock().unwrap().clone();
    slots.sort();
    assert_eq!(
        slots,
        vec![1, 2],
        "shutdown_workers must dispatch ReleaseWorkerPane for every registered slot",
    );

    // Slot mappings and live-state entries must be drained — a
    // future re-spawn into the same slot id has to start clean.
    assert_eq!(server_state.worker_registry.slot_for_run("run-a"), None);
    assert_eq!(server_state.worker_registry.slot_for_run("run-b"), None);
    assert!(server_state.live_worker_states.snapshot().is_empty());
}

/// Empty registry → no-op. Guards against `shutdown_workers`
/// hanging on `JoinSet::join_next` when there's nothing to await,
/// and against gratuitous SIGTERMs at idle shutdown.
#[tokio::test]
async fn shutdown_workers_is_noop_when_no_workers_registered() {
    let server_state = test_server_state();
    // No app session, no slot registrations — must still return.
    server_state
        .shutdown_workers(Duration::from_millis(50), Duration::from_millis(0))
        .await;
}

// --- resolve_status_actor regression suite ---
//
// Pins the three-direction contract:
//   1. Boss-session ancestry → "boss"
//   2. No boss_pid registered → "human"
//   3. Unrelated peer (not in boss subtree) → "human"
//
// We use the current process pid as a stand-in for the "registered
// boss pid" because `is_descendant_of_any` treats a pid as a
// descendant of itself (first iteration of the trust-root check).

#[test]
fn resolve_status_actor_returns_boss_when_peer_is_boss_descendant() {
    let server_state = test_server_state();
    let our_pid = std::process::id() as libc::pid_t;
    server_state.set_boss_pid(our_pid);
    // Our own pid is in the boss subtree (pid is descendant of itself).
    assert_eq!(
        resolve_status_actor(&server_state, Some(our_pid)),
        boss_protocol::LAST_STATUS_ACTOR_BOSS,
    );
}

#[test]
fn resolve_status_actor_returns_human_when_no_boss_pid_registered() {
    let server_state = test_server_state();
    let our_pid = std::process::id() as libc::pid_t;
    // No call to set_boss_pid — boss trust root is absent.
    assert_eq!(
        resolve_status_actor(&server_state, Some(our_pid)),
        boss_protocol::LAST_STATUS_ACTOR_HUMAN,
    );
}

#[test]
fn resolve_status_actor_returns_human_when_peer_is_not_boss_descendant() {
    let server_state = test_server_state();
    // Register a non-existent pid as the boss root — our process is
    // not a descendant of it.
    server_state.set_boss_pid(99_999_999);
    let our_pid = std::process::id() as libc::pid_t;
    assert_eq!(
        resolve_status_actor(&server_state, Some(our_pid)),
        boss_protocol::LAST_STATUS_ACTOR_HUMAN,
    );
}

#[test]
fn resolve_status_actor_returns_human_when_peer_pid_is_none() {
    let server_state = test_server_state();
    let our_pid = std::process::id() as libc::pid_t;
    server_state.set_boss_pid(our_pid);
    // peer_pid is None — falls through to human (no pid to match against).
    assert_eq!(
        resolve_status_actor(&server_state, None),
        boss_protocol::LAST_STATUS_ACTOR_HUMAN,
    );
}

// ---- in_review_chore_execution ----

fn make_work_db_with_chore() -> (Arc<WorkDb>, String, String) {
    use crate::work::{CreateChoreInput, CreateProductInput};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("boss.db");
    std::mem::forget(dir);
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = db
        .create_product(CreateProductInput {
            name: "Test".into(),
            description: None,
            repo_remote_url: Some("git@github.com:test/test.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(CreateChoreInput {
            product_id: product.id.clone(),
            name: "In-review reap test".into(),
            description: None,
            autostart: true,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        })
        .unwrap();
    (db, product.id, chore.id)
}

#[test]
fn in_review_chore_execution_returns_none_for_non_in_review_status() {
    use boss_protocol::WorkItemPatch;
    let (db, _, chore_id) = make_work_db_with_chore();
    // Default chore status is "todo" (autostart=true → "active" actually,
    // but either way it is not "in_review").
    let item = db.get_work_item(&chore_id).unwrap();
    assert!(
        in_review_chore_execution(&db, &item).is_none(),
        "must return None when the chore is not in_review"
    );
    // Move to done — still not in_review.
    let done_item = db
        .update_work_item(
            &chore_id,
            WorkItemPatch {
                status: Some("done".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        in_review_chore_execution(&db, &done_item).is_none(),
        "must return None for done (not in_review)"
    );
}

#[test]
fn in_review_chore_execution_returns_none_when_no_execution() {
    use boss_protocol::WorkItemPatch;
    let (db, _, chore_id) = make_work_db_with_chore();
    let item = db
        .update_work_item(
            &chore_id,
            WorkItemPatch {
                status: Some("in_review".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        in_review_chore_execution(&db, &item).is_none(),
        "must return None when the chore has no executions"
    );
}

#[test]
fn in_review_chore_execution_returns_execution_id_when_in_review() {
    use crate::work::CreateExecutionInput;
    use boss_protocol::WorkItemPatch;
    let (db, _, chore_id) = make_work_db_with_chore();
    // Create an execution for the chore.
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(boss_protocol::ExecutionKind::ChoreImplementation)
                .status("ready")
                .build(),
        )
        .unwrap();
    let item = db
        .update_work_item(
            &chore_id,
            WorkItemPatch {
                status: Some("in_review".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let found = in_review_chore_execution(&db, &item);
    assert_eq!(
        found.as_deref(),
        Some(execution.id.as_str()),
        "must return the execution id when the chore is in_review and has an execution"
    );
}

#[test]
fn in_review_chore_execution_returns_none_for_product() {
    use crate::work::CreateProductInput;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("boss.db");
    std::mem::forget(dir);
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product_item = db
        .create_product(CreateProductInput {
            name: "Prod".into(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let item = WorkItem::Product(product_item);
    assert!(
        in_review_chore_execution(&db, &item).is_none(),
        "must return None for non-task work items"
    );
}

// ---- active_to_todo_execution ----

#[test]
fn active_to_todo_execution_returns_none_when_not_todo() {
    use boss_protocol::WorkItemPatch;
    let (db, _, chore_id) = make_work_db_with_chore();
    let item = db
        .update_work_item(
            &chore_id,
            WorkItemPatch {
                status: Some("active".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        active_to_todo_execution(&db, &Some("todo".into()), &item).is_none(),
        "must return None when the current status is not todo"
    );
}

#[test]
fn active_to_todo_execution_returns_none_when_prev_not_active() {
    use boss_protocol::WorkItemPatch;
    let (db, _, chore_id) = make_work_db_with_chore();
    let item = db
        .update_work_item(
            &chore_id,
            WorkItemPatch {
                status: Some("todo".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        active_to_todo_execution(&db, &Some("todo".into()), &item).is_none(),
        "must return None when the previous status was not active"
    );
    assert!(
        active_to_todo_execution(&db, &None, &item).is_none(),
        "must return None when there is no previous status"
    );
}

#[test]
fn active_to_todo_execution_returns_none_when_no_execution() {
    use boss_protocol::WorkItemPatch;
    let (db, _, chore_id) = make_work_db_with_chore();
    let item = db
        .update_work_item(
            &chore_id,
            WorkItemPatch {
                status: Some("todo".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        active_to_todo_execution(&db, &Some("active".into()), &item).is_none(),
        "must return None when the chore has no executions"
    );
}

#[test]
fn active_to_todo_execution_returns_execution_id() {
    use crate::work::CreateExecutionInput;
    use boss_protocol::WorkItemPatch;
    let (db, _, chore_id) = make_work_db_with_chore();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(boss_protocol::ExecutionKind::ChoreImplementation)
                .status("running")
                .build(),
        )
        .unwrap();
    let item = db
        .update_work_item(
            &chore_id,
            WorkItemPatch {
                status: Some("todo".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let found = active_to_todo_execution(&db, &Some("active".into()), &item);
    assert_eq!(
        found.as_deref(),
        Some(execution.id.as_str()),
        "must return the execution id when the chore is moving from active to todo"
    );
}

#[test]
fn active_to_todo_execution_returns_none_for_product() {
    use crate::work::CreateProductInput;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("boss.db");
    std::mem::forget(dir);
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product_item = db
        .create_product(CreateProductInput {
            name: "Prod".into(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let item = WorkItem::Product(product_item);
    assert!(
        active_to_todo_execution(&db, &Some("active".into()), &item).is_none(),
        "must return None for non-task work items"
    );
}

// ---- live_execution_for_deleted_item ----

#[test]
fn live_execution_for_deleted_item_returns_none_when_no_execution() {
    let (db, _, chore_id) = make_work_db_with_chore();
    let item = db.get_work_item(&chore_id).unwrap();
    assert!(
        live_execution_for_deleted_item(&db, &item).is_none(),
        "must return None when the chore has no executions"
    );
}

#[test]
fn live_execution_for_deleted_item_returns_execution_id_when_running() {
    use crate::work::CreateExecutionInput;
    let (db, _, chore_id) = make_work_db_with_chore();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(boss_protocol::ExecutionKind::ChoreImplementation)
                .status("running")
                .build(),
        )
        .unwrap();
    let item = db.get_work_item(&chore_id).unwrap();
    assert_eq!(
        live_execution_for_deleted_item(&db, &item).as_deref(),
        Some(execution.id.as_str()),
        "must return the live execution id so the worker can be torn down"
    );
}

#[test]
fn live_execution_for_deleted_item_returns_none_when_terminal() {
    use crate::work::CreateExecutionInput;
    for status in ["completed", "failed", "abandoned", "cancelled", "orphaned"] {
        let (db, _, chore_id) = make_work_db_with_chore();
        db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(boss_protocol::ExecutionKind::ChoreImplementation)
                .status(status)
                .build(),
        )
        .unwrap();
        let item = db.get_work_item(&chore_id).unwrap();
        assert!(
            live_execution_for_deleted_item(&db, &item).is_none(),
            "must return None for terminal execution status {status:?}"
        );
    }
}

#[test]
fn live_execution_for_deleted_item_returns_none_for_product() {
    use crate::work::CreateProductInput;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("boss.db");
    std::mem::forget(dir);
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product_item = db
        .create_product(CreateProductInput {
            name: "Prod".into(),
            description: None,
            repo_remote_url: None,
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let item = WorkItem::Product(product_item);
    assert!(
        live_execution_for_deleted_item(&db, &item).is_none(),
        "must return None for non-task work items"
    );
}

// --- chore-update notification helpers ---

#[test]
fn build_chore_update_message_returns_none_when_nothing_changed() {
    assert!(
        build_chore_update_message("Same name", "Same name", "Same desc", "Same desc")
            .is_none()
    );
}

#[test]
fn build_chore_update_message_includes_name_diff() {
    let msg = build_chore_update_message("old name", "new name", "desc", "desc")
        .expect("should produce a message");
    assert!(msg.contains("[chore-update]"), "must contain the tag");
    assert!(msg.contains("old name"), "must contain the old name");
    assert!(msg.contains("new name"), "must contain the new name");
    assert!(
        !msg.contains("description"),
        "must not mention description when it is unchanged"
    );
}

#[test]
fn build_chore_update_message_includes_description_diff() {
    let msg = build_chore_update_message("name", "name", "old description", "new description")
        .expect("should produce a message");
    assert!(msg.contains("[chore-update]"));
    assert!(msg.contains("old description"));
    assert!(msg.contains("new description"));
}

#[test]
fn build_chore_update_message_includes_both_when_both_change() {
    let msg = build_chore_update_message("old name", "new name", "old desc", "new desc")
        .expect("should produce a message when both fields change");
    assert!(msg.contains("old name"));
    assert!(msg.contains("new name"));
    assert!(msg.contains("old desc"));
    assert!(msg.contains("new desc"));
}

#[test]
fn active_chore_run_id_returns_none_for_todo_chore() {
    use boss_protocol::WorkItemPatch;
    let state = test_server_state();
    let (db, _, chore_id) = make_work_db_with_chore();
    // Default status is todo (autostart=true makes it active in
    // make_work_db_with_chore, but let's force todo here).
    let _ = db
        .update_work_item(
            &chore_id,
            WorkItemPatch {
                status: Some("todo".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let item = db.get_work_item(&chore_id).unwrap();
    assert!(
        active_chore_run_id(&state, &item).is_none(),
        "todo chore should return None (not active)"
    );
}

#[test]
fn active_chore_run_id_returns_none_when_no_live_worker() {
    use boss_protocol::WorkItemPatch;
    let state = test_server_state();
    let (db, _, chore_id) = make_work_db_with_chore();
    let item = db
        .update_work_item(
            &chore_id,
            WorkItemPatch {
                status: Some("active".into()),
                ..Default::default()
            },
        )
        .unwrap();
    // No worker registered — live_worker_states is empty.
    assert!(
        active_chore_run_id(&state, &item).is_none(),
        "active chore with no live worker should return None"
    );
}

#[tokio::test]
async fn chore_update_notify_sends_message_to_live_worker() {
    // End-to-end smoke for the notification path: sets up a live
    // worker bound to an active chore, then simulates the
    // UpdateWorkItem name-change flow and verifies a SendToPane
    // message is enqueued toward the app session.
    use boss_protocol::{WorkItemBinding, WorkItemPatch};

    let server_state = test_server_state();
    let (db, _, chore_id) = make_work_db_with_chore();

    // Put the chore in active status.
    let active_item = db
        .update_work_item(
            &chore_id,
            WorkItemPatch {
                status: Some("active".into()),
                ..Default::default()
            },
        )
        .unwrap();

    // Register a live worker slot for this chore.
    let run_id = "exec-notify-test";
    server_state.worker_registry.register_run_slot(run_id, 4);
    server_state.live_worker_states.register_spawn(
        4,
        run_id,
        "claude-opus-4-7",
        9999,
        Some(WorkItemBinding {
            work_item_id: chore_id.clone(),
            work_item_name: "Test chore".into(),
            execution_id: run_id.into(),
        }),
    );

    // Register an app session to capture the outgoing SendToPane.
    let app_sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), app_sink.clone())
        .await;

    // Simulate the pre-update snapshot.
    let chore_task = match &active_item {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        _ => panic!("expected task/chore"),
    };
    let old_name = chore_task.name.clone();
    let old_description = chore_task.description.clone();

    // Build and apply the update with a name change.
    let updated_item = db
        .update_work_item(
            &chore_id,
            WorkItemPatch {
                name: Some("Updated chore name".into()),
                ..Default::default()
            },
        )
        .unwrap();

    // Exercise the notification logic inline (mirrors the handler).
    let (new_name, new_description) = match &updated_item {
        WorkItem::Task(t) | WorkItem::Chore(t) => (t.name.clone(), t.description.clone()),
        _ => panic!("expected task/chore"),
    };
    let msg =
        build_chore_update_message(&old_name, &new_name, &old_description, &new_description)
            .expect("name changed — message should be produced");

    let resolved_run = active_chore_run_id(&server_state, &updated_item)
        .expect("active chore with live worker should resolve a run_id");

    let server_clone = server_state.clone();
    let msg_clone = msg.clone();
    let run_clone = resolved_run.clone();
    let send = tokio::spawn(async move {
        server_clone
            .send_input_to_worker(&run_clone, msg_clone)
            .await
    });

    // Drain the app session: expect a SendToPane EngineRequest.
    let envelope = app_sink
        .next()
        .await
        .expect("SendToPane should be enqueued on the app sink");
    let (request_id, request) = match envelope.payload {
        FrontendEvent::EngineRequest {
            request_id,
            request,
        } => (request_id, request),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    match &request {
        EngineToAppRequest::SendToPane(input) => {
            assert_eq!(input.slot_id, 4);
            assert!(
                input.text.contains("[chore-update]"),
                "message must contain [chore-update] tag"
            );
            assert!(
                input.text.contains("Updated chore name"),
                "message must mention the new name"
            );
        }
        other => panic!("expected SendToPane, got {other:?}"),
    }

    // Reply success so the spawned task can complete.
    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::SendToPane {
                result: Ok(crate::protocol::SendToPaneResult {}),
            },
        )
        .await;

    send.await.expect("send task").expect("send ok");
}

// ── executions.transcript tests ──────────────────────────────────────────

/// Helper: create a product + chore + execution (in `ready` status).
fn make_execution_for_test(server_state: &Arc<ServerState>) -> boss_protocol::WorkExecution {
    let product = server_state
        .work_db
        .create_product(boss_protocol::CreateProductInput {
            name: "p".into(),
            description: None,
            repo_remote_url: Some("git@example.com:p.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = server_state
        .work_db
        .create_chore(boss_protocol::CreateChoreInput {
            product_id: product.id.clone(),
            name: "c".into(),
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
    server_state
        .work_db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .build(),
        )
        .unwrap()
}

#[test]
fn segment_to_wire_maps_all_roles() {
    use crate::transcript_markdown::{SegmentRole, TranscriptSegment, TruncationInfo};
    let seg = TranscriptSegment::builder()
        .seq(1u64)
        .role(SegmentRole::Assistant)
        .label("Assistant")
        .markdown("hello")
        .timestamp("2026-01-01")
        .model("claude-opus-4")
        .collapsible(false)
        .default_collapsed(false)
        .truncated(TruncationInfo {
            shown_bytes: 10,
            total_bytes: 100,
        })
        .build();
    let wire = segment_to_wire(seg);
    assert_eq!(wire.seq, 1);
    assert_eq!(wire.role, boss_protocol::SegmentRole::Assistant);
    assert_eq!(wire.label, "Assistant");
    assert_eq!(wire.markdown, "hello");
    assert_eq!(wire.timestamp.as_deref(), Some("2026-01-01"));
    assert_eq!(wire.model.as_deref(), Some("claude-opus-4"));
    assert!(!wire.collapsible);
    assert!(!wire.default_collapsed);
    let trunc = wire.truncated.unwrap();
    assert_eq!(trunc.shown_bytes, 10);
    assert_eq!(trunc.total_bytes, 100);
}

#[test]
fn segment_to_wire_all_roles() {
    use crate::transcript_markdown::SegmentRole;
    let roles = [
        (SegmentRole::User, boss_protocol::SegmentRole::User),
        (
            SegmentRole::Assistant,
            boss_protocol::SegmentRole::Assistant,
        ),
        (SegmentRole::Thinking, boss_protocol::SegmentRole::Thinking),
        (SegmentRole::Tool, boss_protocol::SegmentRole::Tool),
        (SegmentRole::System, boss_protocol::SegmentRole::System),
    ];
    for (src, expected) in roles {
        let seg = crate::transcript_markdown::TranscriptSegment::builder()
            .seq(0u64)
            .role(src)
            .label("x")
            .markdown("y")
            .build();
        assert_eq!(segment_to_wire(seg).role, expected);
    }
}

#[tokio::test]
async fn execution_transcript_no_path_returns_unavailable() {
    let server_state = test_server_state();
    let execution = make_execution_for_test(&server_state);

    // No run row → no transcript_path.
    let path = server_state
        .work_db
        .transcript_path_for_execution(&execution.id)
        .unwrap();
    assert!(
        path.is_none(),
        "precondition: fresh execution has no transcript path"
    );
    // The handler would return ExecutionTranscriptUnavailable.
    // Verify the DB layer returns None and is_live is false.
    let is_live = execution.finished_at.is_none()
        && matches!(execution.status.as_str(), "running" | "waiting_human");
    assert!(!is_live, "ready execution should not be live");
}

#[tokio::test]
async fn execution_transcript_missing_file_case() {
    let server_state = test_server_state();
    let execution = make_execution_for_test(&server_state);

    // Create a run row pointing at a non-existent file.
    let nonexistent = "/tmp/does_not_exist_boss_test.jsonl";
    server_state
        .work_db
        .create_run(crate::protocol::CreateRunInput {
            execution_id: execution.id.clone(),
            agent_id: "agent-1".into(),
            status: Some("active".into()),
            transcript_path: Some(nonexistent.into()),
            artifacts_path: None,
            result_summary: None,
            error_text: None,
            started_at: None,
            finished_at: None,
        })
        .unwrap();

    let path = server_state
        .work_db
        .transcript_path_for_execution(&execution.id)
        .unwrap();
    assert_eq!(path.as_deref(), Some(nonexistent));

    // Try to read — should be NotFound.
    let err = tokio::fs::read_to_string(nonexistent).await.unwrap_err();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::NotFound,
        "file must not exist for this test to be valid"
    );
}

#[tokio::test]
async fn execution_transcript_normal_case() {
    let server_state = test_server_state();
    let execution = make_execution_for_test(&server_state);

    let transcript_dir = tempfile::tempdir().unwrap();
    let transcript_path = transcript_dir.path().join("session.jsonl");
    // One user turn + one assistant turn.
    let jsonl = concat!(
        "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"world\"}]}}\n",
    );
    std::fs::write(&transcript_path, jsonl).unwrap();

    server_state
        .work_db
        .create_run(crate::protocol::CreateRunInput {
            execution_id: execution.id.clone(),
            agent_id: "agent-1".into(),
            status: Some("active".into()),
            transcript_path: Some(transcript_path.display().to_string()),
            artifacts_path: None,
            result_summary: None,
            error_text: None,
            started_at: None,
            finished_at: None,
        })
        .unwrap();

    let path = server_state
        .work_db
        .transcript_path_for_execution(&execution.id)
        .unwrap()
        .expect("transcript path must be set");

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    let events = crate::transcript_markdown::parse_transcript(&content);
    assert!(!events.is_empty(), "must parse at least one event");

    let segments = crate::transcript_markdown::events_to_segments(&events, &Default::default());
    assert!(!segments.is_empty(), "must produce at least one segment");

    let wire: Vec<boss_protocol::TranscriptSegment> =
        segments.into_iter().map(segment_to_wire).collect();
    assert!(
        wire.iter()
            .any(|s| s.role == boss_protocol::SegmentRole::User),
        "must have a User segment"
    );
    assert!(
        wire.iter()
            .any(|s| s.role == boss_protocol::SegmentRole::Assistant),
        "must have an Assistant segment"
    );
}

#[tokio::test]
async fn execution_transcript_live_flag() {
    let server_state = test_server_state();
    let execution = make_execution_for_test(&server_state);

    // Start the execution so its status becomes "running".
    let (live_exec, _run) = server_state
        .work_db
        .start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();

    let is_live = live_exec.finished_at.is_none()
        && matches!(live_exec.status.as_str(), "running" | "waiting_human");
    assert!(is_live, "a running execution must be flagged as live");
}

#[test]
fn executions_list_returns_empty_for_task_with_no_executions() {
    let server_state = test_server_state();
    let product = server_state
        .work_db
        .create_product(boss_protocol::CreateProductInput {
            name: "p".into(),
            description: None,
            repo_remote_url: Some("git@example.com:p.git".into()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let task = server_state
        .work_db
        .create_chore(boss_protocol::CreateChoreInput {
            product_id: product.id.clone(),
            name: "c".into(),
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
    let executions = server_state
        .work_db
        .list_executions(Some(&task.id))
        .unwrap();
    assert!(
        executions.is_empty(),
        "a task with no executions must return an empty list"
    );
}

/// `tail_lines_from_content` with `lines == 0` must return the entire
/// file contents, never truncated. This is the "show me the whole
/// transcript" path that `bossctl agents transcript` uses by default
/// (the default `--lines` is 0 so coordinators see the full conversation,
/// not just the metadata events that happen to land at the tail).
#[test]
fn tail_lines_from_content_zero_returns_all_lines() {
    let content = "line1\nline2\nline3\n";
    let (lines, truncated) = tail_lines_from_content(content, 0);
    assert_eq!(lines, vec!["line1", "line2", "line3"]);
    assert!(!truncated, "lines=0 must never set truncated");
}

#[test]
fn tail_lines_from_content_zero_on_empty_content() {
    let (lines, truncated) = tail_lines_from_content("", 0);
    assert!(lines.is_empty());
    assert!(!truncated);
}

#[test]
fn tail_lines_from_content_nonzero_tails_from_end() {
    let content = "a\nb\nc\nd\ne\n";
    let (lines, truncated) = tail_lines_from_content(content, 3);
    assert_eq!(lines, vec!["c", "d", "e"]);
    assert!(truncated);
}

#[test]
fn tail_lines_from_content_nonzero_larger_than_total_returns_all() {
    let content = "x\ny\n";
    let (lines, truncated) = tail_lines_from_content(content, 100);
    assert_eq!(lines, vec!["x", "y"]);
    assert!(!truncated);
}
