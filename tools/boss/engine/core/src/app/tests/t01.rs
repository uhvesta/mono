use super::*;
use super::super::server::process_group_signal_target;
use super::super::worker_events::extract_last_assistant_text;

#[test]
fn process_group_signal_target_negates_pgid_for_live_pid() {
    // Our own pid is alive and has a valid process group, so the
    // reaper signals the whole group (negated pgid).
    let me = std::process::id() as libc::pid_t;
    let pgid = unsafe { libc::getpgid(me) };
    assert!(pgid > 0, "own pgid should resolve");
    assert_eq!(process_group_signal_target(me), -pgid);
}

#[test]
fn process_group_signal_target_falls_back_to_bare_pid_when_gone() {
    // A pid that cannot exist has no process group; `getpgid` fails
    // and we fall back to signalling the bare pid rather than the
    // group (negating would otherwise target an unrelated group).
    let bogus: libc::pid_t = i32::MAX;
    assert_eq!(process_group_signal_target(bogus), bogus);
}

#[test]
fn reap_worker_process_tree_noop_for_unreported_pid() {
    // `shell_pid <= 0` means the app never reported a pid; the
    // reaper must early-return (no signal, no `tokio::spawn`, so no
    // runtime required) rather than signal pid 0 / a negative pid.
    reap_worker_process_tree(0, Duration::from_secs(5));
    reap_worker_process_tree(-1, Duration::from_secs(5));
}

#[tokio::test]
async fn reap_worker_process_tree_kills_orphan_child() {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    // Spawn a long sleeper in its OWN process group so our reap —
    // which signals the process *group* — cannot touch the test
    // runner's own group.
    let mut child = unsafe {
        Command::new("sleep")
            .arg("300")
            .pre_exec(|| {
                // setpgid(0, 0): become our own process group leader.
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })
            .spawn()
            .expect("spawn sleep child")
    };
    let pid = child.id() as i32;
    assert!(
        matches!(
            crate::dead_pid_sweep::probe_pid(pid),
            crate::dead_pid_sweep::PidStatus::Alive
        ),
        "child should be alive before reap",
    );

    // SIGTERM fires synchronously; the SIGKILL escalation is
    // detached. `sleep` terminates on SIGTERM, so the child dies
    // either way.
    reap_worker_process_tree(pid, Duration::from_millis(50));

    // Block on the child's exit on a blocking thread so the detached
    // escalation task keeps running on the test runtime.
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("join wait task")
        .expect("wait on child");
    assert!(
        !status.success(),
        "child should have been signalled to death, not exited cleanly",
    );
}


#[test]
fn coalesces_same_topic_into_a_single_pending_envelope() {
    let mut q = SessionQueue::new();
    assert_eq!(
        q.enqueue(topic_envelope("work.products", 1)),
        EnqueueOutcome::Enqueued
    );
    assert_eq!(
        q.enqueue(topic_envelope("work.products", 2)),
        EnqueueOutcome::Coalesced
    );
    assert_eq!(
        q.enqueue(topic_envelope("work.products", 3)),
        EnqueueOutcome::Coalesced
    );
    assert_eq!(q.items.len(), 1);
    let env = q.pop_front().unwrap();
    assert_eq!(env.revision, Some(3));
    assert!(q.pop_front().is_none());
}

#[test]
fn does_not_coalesce_across_topics() {
    let mut q = SessionQueue::new();
    q.enqueue(topic_envelope("work.products", 1));
    q.enqueue(topic_envelope("work.product.p1", 2));
    q.enqueue(topic_envelope("work.products", 3));
    assert_eq!(q.items.len(), 2);

    let first = q.pop_front().unwrap();
    let second = q.pop_front().unwrap();
    assert_eq!(topic_of(&first).as_deref(), Some("work.products"));
    assert_eq!(first.revision, Some(3));
    assert_eq!(topic_of(&second).as_deref(), Some("work.product.p1"));
    assert_eq!(second.revision, Some(2));
}

#[test]
fn coalescing_indices_survive_pops_of_other_topics() {
    let mut q = SessionQueue::new();
    q.enqueue(topic_envelope("a", 1));
    q.enqueue(topic_envelope("b", 2));
    // Pop topic "a", then a new "b" event should still coalesce with
    // the earlier "b" sitting at the (now-front) of the queue.
    let popped = q.pop_front().unwrap();
    assert_eq!(topic_of(&popped).as_deref(), Some("a"));
    assert_eq!(q.enqueue(topic_envelope("b", 3)), EnqueueOutcome::Coalesced);
    assert_eq!(q.items.len(), 1);
    assert_eq!(q.pop_front().unwrap().revision, Some(3));
}

#[test]
fn enqueue_marks_slow_when_queue_is_full() {
    let mut q = SessionQueue::new();
    // Fill with non-coalescing responses up to the cap.
    for i in 0..MAX_SESSION_QUEUE {
        assert_eq!(
            q.enqueue(response_envelope(&format!("r-{i}"))),
            EnqueueOutcome::Enqueued
        );
    }
    assert_eq!(
        q.enqueue(response_envelope("overflow")),
        EnqueueOutcome::Slow
    );
    assert!(q.slow);
    // Subsequent enqueues continue to report Slow.
    assert_eq!(
        q.enqueue(response_envelope("after-overflow")),
        EnqueueOutcome::Slow
    );
}

#[test]
fn enqueue_returns_closed_after_close() {
    let mut q = SessionQueue::new();
    q.closed = true;
    assert_eq!(q.enqueue(response_envelope("r-1")), EnqueueOutcome::Closed);
}

#[tokio::test]
async fn sink_next_drains_queue_and_returns_none_when_closed() {
    let (tx, _rx) = oneshot::channel::<()>();
    let sink = Arc::new(SessionSink::new(tx));
    sink.enqueue(response_envelope("r-1"));
    sink.enqueue(response_envelope("r-2"));
    sink.close();

    let first = sink.next().await.expect("first envelope");
    assert_eq!(first.request_id.as_deref(), Some("r-1"));
    let second = sink.next().await.expect("second envelope");
    assert_eq!(second.request_id.as_deref(), Some("r-2"));
    assert!(sink.next().await.is_none());
}

#[tokio::test]
async fn sink_close_wakes_pending_next_call() {
    let (tx, _rx) = oneshot::channel::<()>();
    let sink = Arc::new(SessionSink::new(tx));
    let waiter = sink.clone();
    let join = tokio::spawn(async move { waiter.next().await });
    // Give the spawned task time to enter notified().await.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    sink.close();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), join)
        .await
        .expect("close should wake next()");
    assert!(result.unwrap().is_none());
}

#[tokio::test]
async fn broker_publish_disconnects_slow_subscriber() {
    let (tx, mut rx) = oneshot::channel::<()>();
    let sink = Arc::new(SessionSink::new(tx));

    // Pre-fill the sink past capacity by injecting non-coalescing entries
    // (responses are not coalesced) without ever draining.
    {
        let mut q = sink.queue.lock().unwrap();
        for i in 0..MAX_SESSION_QUEUE {
            let outcome = q.enqueue(response_envelope(&format!("r-{i}")));
            assert_eq!(outcome, EnqueueOutcome::Enqueued);
        }
    }

    let broker = TopicBroker::default();
    broker.register_session("session-1", sink.clone()).await;
    broker
        .subscribe("session-1", &["work.products".to_owned()])
        .await;

    // Publishing one more event should overflow and trigger shutdown.
    broker
        .publish("work.products", topic_envelope("work.products", 99))
        .await;

    let shutdown = tokio::time::timeout(std::time::Duration::from_secs(1), &mut rx)
        .await
        .expect("shutdown should fire");
    assert!(shutdown.is_ok());

    // Broker should also have evicted the session.
    let inner = broker.inner.lock().await;
    assert!(!inner.sinks.contains_key("session-1"));
    assert!(!inner.sessions_by_topic.contains_key("work.products"));
}


/// The engine-health helper must surface a
/// `missing_anthropic_api_key` issue when the agent config
/// resolved with no key — that's exactly the case the macOS app
/// banner exists to flag, and a silent-success regression here
/// would put us right back at the #699 failure mode.
#[tokio::test]
async fn engine_health_report_flags_missing_anthropic_api_key() {
    let state = test_server_state();
    // Pin: the test fixture intentionally builds without an
    // ANTHROPIC_API_KEY so the missing-key arm is exercised.
    assert!(
        state.anthropic_api_key.is_none(),
        "test fixture should construct without ANTHROPIC_API_KEY",
    );

    let report = build_engine_health_report(&state);
    assert!(!report.anthropic_api_key_present);
    assert_eq!(report.issues.len(), 1, "issues: {:?}", report.issues);
    let issue = &report.issues[0];
    assert_eq!(issue.kind, "missing_anthropic_api_key");
    assert_eq!(issue.severity, "warning");
    assert!(
        !issue.title.is_empty() && !issue.body.is_empty(),
        "title and body must be populated so the banner has \
         user-visible text"
    );
}

/// And the symmetric case: when the engine *does* have an API
/// key, the report must be empty so the macOS banner stays
/// hidden.
#[tokio::test]
async fn engine_health_report_is_empty_when_api_key_present() {
    let temp = tempfile::tempdir().unwrap();
    let work = crate::config::WorkConfig::builder().cwd(temp.path().to_path_buf()).db_path(temp.path().join("state.db")).build();
    let agent = crate::config::AgentConfig {
        anthropic_api_key: Some("sk-test".to_owned()),
        cube: crate::config::CubeConfig {
            command: "cube".to_owned(),
            args: vec![],
        },
        cwd: work.cwd.clone(),
    };
    let cfg = Arc::new(RuntimeConfig::from_parts(work, Some(agent)));
    std::mem::forget(temp);
    let state = ServerState::new_arc_with_app_pid(cfg, None, None).unwrap();

    let report = build_engine_health_report(&state);
    assert!(report.anthropic_api_key_present);
    assert!(
        report.issues.is_empty(),
        "healthy engine must report no issues; got {:?}",
        report.issues,
    );
}

/// Regression guard for the version-mismatch restart path (T460
/// + the chore that surfaced this gap): engine startup must
/// call `build_info::init()` so the binary-fingerprint OnceLock
/// is pinned to the bytes the engine launched from. Without
/// this, an in-place app upgrade could rewrite the engine's
/// own binary on disk before the first GetEngineVersion query,
/// causing the running (old) engine to report the *new*
/// fingerprint and the app to silently attach to the stale
/// engine instead of restarting it.
#[tokio::test]
async fn engine_startup_eagerly_initializes_binary_fingerprint() {
    crate::build_info::reset_eager_init_for_test();
    let _state = test_server_state();
    assert!(
        crate::build_info::eager_init_called_for_test(),
        "build_info::init() must be called during ServerState construction; \
         removing the call breaks the macOS app version-mismatch restart path"
    );
}

/// Wire-shape regression for the GetEngineVersion handler: the
/// macOS app sends a raw `{"request_id":"version-check",
/// "payload":{"type":"get_engine_version"}}` frame (no session
/// registration) and parses the response by reading the
/// top-level `request_id`, `payload.type` == "engine_version_result",
/// and `payload.binary_fingerprint`. If serde tags or envelope
/// names ever change, the Swift parser silently returns nil and
/// the version check is skipped — which looks just like an old
/// engine that doesn't speak the verb. This test holds the
/// contract pinned to the bytes-on-the-wire the Swift code
/// expects.
#[tokio::test]
async fn get_engine_version_response_matches_swift_app_parser() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let server_state = test_server_state();
    let (engine_side, app_side) = tokio::net::UnixStream::pair().unwrap();
    let conn = tokio::spawn(handle_frontend_connection(engine_side, server_state, None));

    let (read_half, mut write_half) = app_side.into_split();
    let mut reader = BufReader::new(read_half);

    // Drain the initial Hello push the engine emits on connect.
    let mut hello = String::new();
    reader.read_line(&mut hello).await.unwrap();
    let hello_json: serde_json::Value = serde_json::from_str(&hello).unwrap();
    assert_eq!(hello_json["payload"]["type"], "hello");

    // Send the exact byte sequence EngineProcessController.swift
    // emits. Using a literal here (not a Rust struct) so a serde
    // refactor that broke wire compatibility couldn't sneak past
    // a round-trip test.
    let request =
        b"{\"request_id\":\"version-check\",\"payload\":{\"type\":\"get_engine_version\"}}\n";
    write_half.write_all(request).await.unwrap();
    write_half.flush().await.unwrap();

    let mut response = String::new();
    reader.read_line(&mut response).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(parsed["request_id"], "version-check");
    assert_eq!(parsed["payload"]["type"], "engine_version_result");
    let fp = parsed["payload"]["binary_fingerprint"]
        .as_str()
        .expect("binary_fingerprint must be a string");
    assert!(!fp.is_empty());
    assert!(parsed["payload"]["git_sha"].is_string());
    assert!(parsed["payload"]["build_time"].is_string());

    // Drop the writer so the engine-side reader unblocks and the
    // task exits without us having to call any shutdown verb.
    drop(write_half);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), conn).await;
}

#[tokio::test]
async fn send_to_app_returns_not_registered_when_no_app() {
    let server_state = test_server_state();
    let result = server_state
        .send_to_app(
            EngineToAppRequest::SpawnWorkerPane(crate::protocol::SpawnWorkerPaneInput {
                run_id: "r".into(),
                workspace_path: "/tmp".into(),
                slot_id: 1,
                initial_input: "claude\n".into(),
                env: vec![],
                summary: None,
                task_title: None,
            }),
            Duration::from_millis(50),
        )
        .await;
    assert!(matches!(result, Err(SendToAppError::NotRegistered)));
}

#[tokio::test]
async fn send_to_app_round_trips_via_deliver_response() {
    let server_state = test_server_state();
    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let send = tokio::spawn(async move {
        server_clone
            .send_to_app(
                EngineToAppRequest::SpawnWorkerPane(crate::protocol::SpawnWorkerPaneInput {
                    run_id: "run-7".into(),
                    workspace_path: "/tmp".into(),
                    slot_id: 1,
                    initial_input: "claude\n".into(),
                    env: vec![],
                    summary: None,
                    task_title: None,
                }),
                Duration::from_secs(2),
            )
            .await
    });

    // Pull the EngineRequest event off the sink; that gives us
    // the request_id the engine assigned.
    let envelope = sink
        .next()
        .await
        .expect("an EngineRequest event should be enqueued");
    let request_id = match &envelope.payload {
        FrontendEvent::EngineRequest { request_id, .. } => request_id.clone(),
        other => panic!("expected EngineRequest, got {other:?}"),
    };

    // Deliver a response for that id.
    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::SpawnWorkerPane {
                result: Ok(crate::protocol::SpawnWorkerPaneResult {
                    slot_id: 4,
                    shell_pid: 9001,
                }),
            },
        )
        .await;

    let response = send.await.expect("send_to_app task panicked").expect("ok");
    match response {
        EngineToAppResponse::SpawnWorkerPane { result } => {
            let result = result.expect("ok variant");
            assert_eq!(result.slot_id, 4);
            assert_eq!(result.shell_pid, 9001);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[tokio::test]
async fn send_to_app_resolves_app_disconnected_on_session_drop() {
    let server_state = test_server_state();
    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let send = tokio::spawn(async move {
        server_clone
            .send_to_app(
                EngineToAppRequest::ReleaseWorkerPane(
                    crate::protocol::ReleaseWorkerPaneInput {
                        slot_id: 1,
                        kill_grace_seconds: 2,
                    },
                ),
                Duration::from_secs(5),
            )
            .await
    });

    // Drain the EngineRequest event so the test isn't racy on
    // sink ordering.
    let _ = sink.next().await;

    // Simulate the app session disconnecting.
    server_state
        .drop_app_session_if_matches("session-app")
        .await;

    let response = send.await.expect("send task panicked").expect("ok");
    match response {
        EngineToAppResponse::SpawnWorkerPane {
            result: Err(EngineToAppError::AppDisconnected),
        } => {} // currently the cleanup path uses SpawnWorkerPane variant uniformly; ok.
        EngineToAppResponse::ReleaseWorkerPane {
            result: Err(EngineToAppError::AppDisconnected),
        } => {}
        other => panic!("expected AppDisconnected, got {other:?}"),
    }
}

#[tokio::test]
async fn send_to_app_times_out_when_app_silent() {
    let server_state = test_server_state();
    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink)
        .await;

    let result = server_state
        .send_to_app(
            EngineToAppRequest::SpawnWorkerPane(crate::protocol::SpawnWorkerPaneInput {
                run_id: "r".into(),
                workspace_path: "/tmp".into(),
                slot_id: 1,
                initial_input: "claude\n".into(),
                env: vec![],
                summary: None,
                task_title: None,
            }),
            Duration::from_millis(50),
        )
        .await;
    assert!(matches!(result, Err(SendToAppError::Timeout)));
}

#[tokio::test]
async fn second_register_invalidates_first() {
    let server_state = test_server_state();
    let first_sink = make_session_sink();
    server_state
        .register_app_session("session-1".into(), first_sink.clone())
        .await;

    let server_clone = server_state.clone();
    let in_flight = tokio::spawn(async move {
        server_clone
            .send_to_app(
                EngineToAppRequest::SpawnWorkerPane(crate::protocol::SpawnWorkerPaneInput {
                    run_id: "r".into(),
                    workspace_path: "/tmp".into(),
                    slot_id: 1,
                    initial_input: "claude\n".into(),
                    env: vec![],
                    summary: None,
                    task_title: None,
                }),
                Duration::from_secs(5),
            )
            .await
    });
    let _ = first_sink.next().await; // drain queued event

    // A second registration replaces the first and resolves
    // pending requests as AppDisconnected.
    let second_sink = make_session_sink();
    server_state
        .register_app_session("session-2".into(), second_sink)
        .await;

    let response = in_flight.await.expect("send task").expect("ok");
    match response {
        EngineToAppResponse::SpawnWorkerPane {
            result: Err(EngineToAppError::AppDisconnected),
        } => {}
        other => panic!("expected AppDisconnected, got {other:?}"),
    }
}

#[tokio::test]
async fn spawn_worker_pane_requests_are_serialized() {
    // Two concurrent SpawnWorkerPane calls go through
    // `WorkerSpawner::send_to_app_request`. The mutex inside that
    // path should ensure only one is enqueued on the sink before
    // the first response is delivered. The second request must
    // not appear in the queue until after the first has resolved.
    use crate::spawn_flow::WorkerSpawner;

    let server_state = test_server_state();
    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let make_request = |run: &str| {
        EngineToAppRequest::SpawnWorkerPane(crate::protocol::SpawnWorkerPaneInput {
            run_id: run.to_owned(),
            workspace_path: "/tmp".into(),
            slot_id: 1,
            initial_input: "claude\n".into(),
            env: vec![],
            summary: None,
            task_title: None,
        })
    };

    let server_a = server_state.clone();
    let send_a = tokio::spawn(async move {
        server_a
            .send_to_app_request(make_request("run-a"), Duration::from_secs(5))
            .await
    });
    let server_b = server_state.clone();
    let send_b = tokio::spawn(async move {
        server_b
            .send_to_app_request(make_request("run-b"), Duration::from_secs(5))
            .await
    });

    // The first request must be on the sink; the second must be
    // gated behind the spawn_pane_lock until the first resolves.
    let first = sink.next().await.expect("first EngineRequest enqueued");
    let first_request_id = match &first.payload {
        FrontendEvent::EngineRequest { request_id, .. } => request_id.clone(),
        other => panic!("expected EngineRequest, got {other:?}"),
    };

    // Give the runtime time to schedule the second task. With
    // serialization the sink stays empty; without it the second
    // request would already be enqueued and `sink.next()` would
    // resolve before the timeout fires.
    let peek = tokio::time::timeout(Duration::from_millis(100), sink.next()).await;
    assert!(
        peek.is_err(),
        "second SpawnWorkerPane should not be in flight while the first is pending; got {:?}",
        peek.ok().flatten().map(|env| env.payload),
    );

    // Resolve the first response — this releases the mutex and
    // lets the second request go.
    server_state
        .deliver_app_response(
            "session-app",
            &first_request_id,
            EngineToAppResponse::SpawnWorkerPane {
                result: Ok(crate::protocol::SpawnWorkerPaneResult {
                    slot_id: 1,
                    shell_pid: 0,
                }),
            },
        )
        .await;
    send_a.await.expect("send_a task").expect("ok response");

    let second = sink.next().await.expect("second EngineRequest enqueued");
    let second_request_id = match &second.payload {
        FrontendEvent::EngineRequest { request_id, .. } => request_id.clone(),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    server_state
        .deliver_app_response(
            "session-app",
            &second_request_id,
            EngineToAppResponse::SpawnWorkerPane {
                result: Ok(crate::protocol::SpawnWorkerPaneResult {
                    slot_id: 2,
                    shell_pid: 0,
                }),
            },
        )
        .await;
    send_b.await.expect("send_b task").expect("ok response");
}

#[tokio::test]
async fn release_worker_pane_drops_live_worker_state() {
    // Regression: chore-done (and other engine-driven release
    // paths) must clear the live-state entry so the UI stops
    // rendering the worker as attached to its work item. Without
    // this, the kanban Doing dot and the pane titlebar pill stayed
    // pinned at the worker's last activity (e.g. WaitingForInput)
    // even after the libghostty pane was torn down.
    let server_state = test_server_state();
    server_state.worker_registry.register_run_slot("run-x", 1);
    server_state
        .live_worker_states
        .register_spawn(1, "run-x", "claude-opus-4-7", 0, None);
    assert!(
        server_state.live_worker_states.get(1).is_some(),
        "precondition: live state for slot 1 should be registered",
    );

    // No app session is registered, so the SendToApp call in
    // release_worker_pane returns NotRegistered. The cleanup must
    // run regardless.
    server_state.release_worker_pane("run-x").await;

    assert!(
        server_state.live_worker_states.get(1).is_none(),
        "release_worker_pane must drop the live-state entry alongside the libghostty pane",
    );
    assert_eq!(
        server_state.worker_registry.slot_for_run("run-x"),
        None,
        "release_worker_pane must drop the worker_registry slot mapping",
    );

    // Idempotent: a second call (e.g. completion-detection then
    // chore-done firing for the same run) is a no-op.
    server_state.release_worker_pane("run-x").await;
    assert!(server_state.live_worker_states.get(1).is_none());
}

#[tokio::test]
async fn release_worker_pane_releases_matching_worker_pool_slot() {
    // Engine-side lifecycle pairing: the WorkerPool slot is held
    // for the lifetime of the libghostty pane (not just for the
    // duration of `run_execution`). Tearing the pane down via
    // `release_worker_pane` must hand the pool slot back so a
    // subsequent `claim_worker` can reuse it — otherwise the
    // engine and the app drift apart and the next
    // SpawnWorkerPane gets rejected as SlotBusy.
    let server_state = test_server_state();
    let pool = server_state.execution_coordinator.worker_pool();

    // Pre-claim slot 1 the way the coordinator would, then wire
    // the worker_registry so `release_worker_pane` can resolve
    // the run id back to that slot.
    let claimed = pool
        .claim_worker("exec-1", None)
        .await
        .expect("worker pool starts with one free slot");
    assert_eq!(claimed, "worker-1");
    assert_eq!(pool.idle_count().await, 0);
    server_state.worker_registry.register_run_slot("run-1", 1);

    // No app session is registered, so the SendToApp call inside
    // release_worker_pane bails on NotRegistered — the pool
    // release must still happen.
    server_state.release_worker_pane("run-1").await;

    assert_eq!(
        pool.idle_count().await,
        1,
        "WorkerPool slot must be freed once the libghostty pane is released",
    );
    // And the next claim lands on the same slot.
    let re_claimed = pool
        .claim_worker("exec-2", None)
        .await
        .expect("slot 1 is free");
    assert_eq!(re_claimed, "worker-1");
}

#[tokio::test]
async fn release_worker_pane_pool_release_is_idempotent() {
    // A pane can be released from more than one path (completion
    // handler, force-release, engine shutdown). `take_slot_for_run`
    // is the natural choke point — the second call sees no slot
    // mapping and short-circuits before touching the pool — so a
    // racy double-release must not zero out an unrelated execution
    // that has already re-claimed the slot.
    let server_state = test_server_state();
    let pool = server_state.execution_coordinator.worker_pool();

    let _claimed = pool.claim_worker("exec-1", None).await.unwrap();
    server_state.worker_registry.register_run_slot("run-1", 1);

    server_state.release_worker_pane("run-1").await;
    assert_eq!(pool.idle_count().await, 1);

    // Re-claim the slot for a new execution.
    let claimed_again = pool.claim_worker("exec-2", None).await.unwrap();
    assert_eq!(claimed_again, "worker-1");
    assert_eq!(pool.idle_count().await, 0);

    // A duplicate release for the original run must not steal the
    // slot back from exec-2.
    server_state.release_worker_pane("run-1").await;
    assert_eq!(
        pool.idle_count().await,
        0,
        "duplicate release_worker_pane must not free a slot now held by a different execution",
    );
}

#[tokio::test]
async fn focus_worker_pane_unknown_run_returns_unknown_run() {
    let server_state = test_server_state();
    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink)
        .await;
    let err = server_state
        .focus_worker_pane("never-allocated")
        .await
        .expect_err("unknown run should fail");
    assert!(matches!(err, FocusPaneError::UnknownRun));
}

#[tokio::test]
async fn focus_worker_pane_round_trips_to_app() {
    // End-to-end smoke: engine resolves run_id → slot via the
    // worker registry, sends a FocusWorkerPane EngineRequest to
    // the registered app session, and surfaces the slot id once
    // the app replies success.
    let server_state = test_server_state();
    server_state
        .worker_registry
        .register_run_slot("run-focus", 5);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let focus = tokio::spawn(async move { server_clone.focus_worker_pane("run-focus").await });

    let envelope = sink
        .next()
        .await
        .expect("an EngineRequest event should be enqueued");
    let (request_id, request) = match envelope.payload {
        FrontendEvent::EngineRequest {
            request_id,
            request,
        } => (request_id, request),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    match request {
        EngineToAppRequest::FocusWorkerPane(input) => {
            assert_eq!(input.slot_id, 5);
        }
        other => panic!("expected FocusWorkerPane, got {other:?}"),
    }

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::FocusWorkerPane {
                result: Ok(crate::protocol::FocusWorkerPaneResult {}),
            },
        )
        .await;

    let slot = focus.await.expect("focus task").expect("focus ok");
    assert_eq!(slot, 5);
}

#[tokio::test]
async fn focus_worker_pane_surfaces_app_error() {
    let server_state = test_server_state();
    server_state
        .worker_registry
        .register_run_slot("run-focus", 3);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let focus = tokio::spawn(async move { server_clone.focus_worker_pane("run-focus").await });

    let envelope = sink.next().await.expect("EngineRequest enqueued");
    let request_id = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, .. } => request_id,
        other => panic!("expected EngineRequest, got {other:?}"),
    };

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::FocusWorkerPane {
                result: Err(EngineToAppError::UnknownSlot),
            },
        )
        .await;

    let err = focus.await.expect("focus task").expect_err("expect err");
    match err {
        FocusPaneError::App(EngineToAppError::UnknownSlot) => {}
        other => panic!("expected App(UnknownSlot), got {other:?}"),
    }
}

#[tokio::test]
async fn send_input_to_worker_unknown_run_returns_unknown_run() {
    let server_state = test_server_state();
    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink)
        .await;
    let err = server_state
        .send_input_to_worker("never-allocated".into(), "/help\n".into())
        .await
        .expect_err("unknown run should fail");
    assert!(matches!(err, SendInputError::UnknownRun));
}

#[tokio::test]
async fn send_input_to_worker_round_trips_to_app() {
    // End-to-end smoke: engine resolves run_id → slot via the
    // worker registry, sends a SendToPane EngineRequest carrying
    // the text payload to the registered app session, and
    // surfaces the slot id once the app replies success.
    let server_state = test_server_state();
    server_state
        .worker_registry
        .register_run_slot("run-send", 7);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let send = tokio::spawn(async move {
        server_clone
            .send_input_to_worker("run-send".into(), "/help\n".into())
            .await
    });

    let envelope = sink
        .next()
        .await
        .expect("an EngineRequest event should be enqueued");
    let (request_id, request) = match envelope.payload {
        FrontendEvent::EngineRequest {
            request_id,
            request,
        } => (request_id, request),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    match request {
        EngineToAppRequest::SendToPane(input) => {
            assert_eq!(input.slot_id, 7);
            assert_eq!(input.text, "/help\n");
        }
        other => panic!("expected SendToPane, got {other:?}"),
    }

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::SendToPane {
                result: Ok(crate::protocol::SendToPaneResult {}),
            },
        )
        .await;

    let slot = send.await.expect("send task").expect("send ok");
    assert_eq!(slot, 7);
}

#[tokio::test]
async fn send_input_to_worker_surfaces_app_error() {
    let server_state = test_server_state();
    server_state
        .worker_registry
        .register_run_slot("run-send", 2);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let send = tokio::spawn(async move {
        server_clone
            .send_input_to_worker("run-send".into(), "hi\n".into())
            .await
    });

    let envelope = sink.next().await.expect("EngineRequest enqueued");
    let request_id = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, .. } => request_id,
        other => panic!("expected EngineRequest, got {other:?}"),
    };

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::SendToPane {
                result: Err(EngineToAppError::UnknownSlot),
            },
        )
        .await;

    let err = send.await.expect("send task").expect_err("expect err");
    match err {
        SendInputError::App(EngineToAppError::UnknownSlot) => {}
        other => panic!("expected App(UnknownSlot), got {other:?}"),
    }
}

#[tokio::test]
async fn interrupt_worker_pane_unknown_run_returns_unknown_run() {
    let server_state = test_server_state();
    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink)
        .await;
    let err = server_state
        .interrupt_worker_pane("never-allocated")
        .await
        .expect_err("unknown run should fail");
    assert!(matches!(err, InterruptPaneError::UnknownRun));
}

#[tokio::test]
async fn interrupt_worker_pane_round_trips_to_app() {
    // End-to-end smoke: engine resolves run_id → slot via the
    // worker registry, sends an InterruptWorkerPane EngineRequest
    // to the registered app session, and surfaces the slot id
    // once the app replies success.
    let server_state = test_server_state();
    server_state.worker_registry.register_run_slot("run-int", 6);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let interrupt =
        tokio::spawn(async move { server_clone.interrupt_worker_pane("run-int").await });

    let envelope = sink
        .next()
        .await
        .expect("an EngineRequest event should be enqueued");
    let (request_id, request) = match envelope.payload {
        FrontendEvent::EngineRequest {
            request_id,
            request,
        } => (request_id, request),
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    match request {
        EngineToAppRequest::InterruptWorkerPane(input) => {
            assert_eq!(input.slot_id, 6);
        }
        other => panic!("expected InterruptWorkerPane, got {other:?}"),
    }

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::InterruptWorkerPane {
                result: Ok(crate::protocol::InterruptWorkerPaneResult {}),
            },
        )
        .await;

    let slot = interrupt
        .await
        .expect("interrupt task")
        .expect("interrupt ok");
    assert_eq!(slot, 6);
}

#[tokio::test]
async fn interrupt_worker_pane_surfaces_app_error() {
    let server_state = test_server_state();
    server_state.worker_registry.register_run_slot("run-int", 2);

    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;

    let server_clone = server_state.clone();
    let interrupt =
        tokio::spawn(async move { server_clone.interrupt_worker_pane("run-int").await });

    let envelope = sink.next().await.expect("EngineRequest enqueued");
    let request_id = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, .. } => request_id,
        other => panic!("expected EngineRequest, got {other:?}"),
    };

    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::InterruptWorkerPane {
                result: Err(EngineToAppError::UnknownSlot),
            },
        )
        .await;

    let err = interrupt
        .await
        .expect("interrupt task")
        .expect_err("expect err");
    match err {
        InterruptPaneError::App(EngineToAppError::UnknownSlot) => {}
        other => panic!("expected App(UnknownSlot), got {other:?}"),
    }
}

#[test]
fn authorize_user_tier_always_allowed() {
    let server_state = test_server_state();
    assert!(server_state.authorize_rpc(RpcTier::User, None));
    assert!(server_state.authorize_rpc(RpcTier::User, Some(1234)));
}

#[test]
fn authorize_no_trust_roots_is_permissive_for_test_mode() {
    let server_state = test_server_state();
    // In tests, both app_pid and boss_pid are None — the engine
    // treats this as permissive so unit tests can drive any RPC.
    assert!(server_state.authorize_rpc(RpcTier::AppOrBoss, Some(1234)));
    assert!(server_state.authorize_rpc(RpcTier::BossOnly, Some(1234)));
}

#[test]
fn set_boss_pid_round_trips() {
    let server_state = test_server_state();
    assert_eq!(server_state.current_boss_pid(), None);
    server_state.set_boss_pid(98765);
    assert_eq!(server_state.current_boss_pid(), Some(98765));
    server_state.set_boss_pid(11111);
    assert_eq!(server_state.current_boss_pid(), Some(11111));
}

#[cfg(target_os = "macos")]
fn server_state_with_app_pid(app_pid: libc::pid_t) -> Arc<ServerState> {
    let temp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RuntimeConfig::from_parts(
        crate::config::WorkConfig::builder().cwd(temp.path().to_path_buf()).db_path(temp.path().join("state.db")).build(),
        None,
    ));
    std::mem::forget(temp);
    ServerState::new_arc_with_app_pid(cfg, Some(app_pid), None).unwrap()
}

#[cfg(target_os = "macos")]
#[test]
fn boss_only_admits_app_descendant_when_boss_pid_unregistered() {
    // Repro for the production bug: macOS app hadn't registered the
    // Boss session pid, so `RpcTier::BossOnly` saw `boss_pid =
    // None`, built an empty trust set, and rejected every caller.
    // The fix: fall back to "descendant of app, not descendant of
    // any registered worker" when boss_pid is unset. The test pid
    // is its own descendant; with app_pid set to it the BossOnly
    // gate must let us through.
    let self_pid = std::process::id() as libc::pid_t;
    let server_state = server_state_with_app_pid(self_pid);
    assert_eq!(server_state.current_boss_pid(), None);
    assert!(
        server_state.authorize_rpc(RpcTier::BossOnly, Some(self_pid)),
        "BossOnly must accept app-descendant callers when boss_pid is unregistered",
    );
}

#[cfg(target_os = "macos")]
#[test]
fn app_or_boss_admits_worker_descendant() {
    // Regression for `bossctl agents stop` rejecting calls made
    // from inside a worker pane. The fix downgrades stop_run from
    // BossOnly to AppOrBoss; AppOrBoss must accept callers that
    // descend from a registered worker shell (workers are
    // siblings under the app), even though BossOnly does not.
    let self_pid = std::process::id() as libc::pid_t;
    let server_state = server_state_with_app_pid(self_pid);
    server_state
        .worker_registry
        .register(self_pid, "fake-run".to_owned());
    assert!(
        server_state.authorize_rpc(RpcTier::AppOrBoss, Some(self_pid)),
        "AppOrBoss must accept worker-pane descendants so `bossctl agents stop` works from a slot",
    );
    // Sanity check: BossOnly still rejects the same caller, so
    // we know the AppOrBoss admission isn't an accidental hole.
    assert!(
        !server_state.authorize_rpc(RpcTier::BossOnly, Some(self_pid)),
        "BossOnly must continue to reject worker-pane descendants",
    );
}

/// Spawn `/usr/bin/true`, wait for it to exit, and return its
/// (now-reaped, definitely-dead) pid. Used to exercise the
/// dead-old-app reattach branch without guessing an unused pid.
#[cfg(target_os = "macos")]
fn reaped_child_pid() -> libc::pid_t {
    let mut child = std::process::Command::new("/usr/bin/true")
        .spawn()
        .expect("spawn /usr/bin/true");
    let pid = child.id() as libc::pid_t;
    child.wait().expect("wait for child to exit");
    pid
}

#[cfg(target_os = "macos")]
#[test]
fn pid_is_alive_true_for_self_false_for_reaped_child() {
    let self_pid = std::process::id() as libc::pid_t;
    assert!(
        pid_is_alive(self_pid),
        "the current process must read as alive"
    );
    assert!(
        !pid_is_alive(0),
        "pid 0 must never read as a live trust root"
    );
    assert!(
        !pid_is_alive(reaped_child_pid()),
        "a reaped child must read as dead"
    );
}

#[test]
fn register_trust_permissive_without_trust_root() {
    // Test / dev mode: no BOSS_APP_PID configured → any peer (even
    // an unknown pid, or none) registers, matching the historical
    // `(None, _) => true` behaviour relied on by unit tests.
    let engine_pid = std::process::id() as libc::pid_t;
    assert!(register_app_session_trust_ok(None, Some(4242), engine_pid));
    assert!(register_app_session_trust_ok(None, None, engine_pid));
}

#[test]
fn register_trust_accepts_matching_pid_and_rejects_unknown_live_pid() {
    let engine_pid = std::process::id() as libc::pid_t;
    let self_pid = std::process::id() as libc::pid_t;
    // Exact match against the pinned app pid → accept.
    assert!(register_app_session_trust_ok(
        Some(self_pid),
        Some(self_pid),
        engine_pid,
    ));
    // A *different* but still-live pid that is neither the trust
    // root nor an engine ancestor must be rejected — this is the
    // guard that stops a second live app hijacking the trust root.
    // (self_pid is alive, so the dead-old-app branch can't fire.)
    let other_live = if self_pid == 2 { 3 } else { 2 };
    assert!(!register_app_session_trust_ok(
        Some(self_pid),
        Some(other_live),
        engine_pid,
    ));
    // A connection with no observable peer pid against a real trust
    // root is rejected.
    assert!(!register_app_session_trust_ok(
        Some(self_pid),
        None,
        engine_pid
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn register_trust_accepts_relaunched_app_when_old_app_pid_is_dead() {
    // The core reattach repro: the engine survived an app restart,
    // so its pinned app pid belongs to a now-dead process, and the
    // relaunched app connects with a fresh, unrelated pid. The new
    // app must be trusted so it can re-register its session —
    // otherwise every engine→app RPC (SpawnWorkerPane, reveal)
    // dies with "no app session is registered". Mirror of T351.
    let engine_pid = std::process::id() as libc::pid_t;
    let dead_old_app = reaped_child_pid();
    let new_app = std::process::id() as libc::pid_t; // a live, unrelated pid
    assert_ne!(dead_old_app, new_app);
    assert!(
        register_app_session_trust_ok(Some(dead_old_app), Some(new_app), engine_pid),
        "a relaunched app must reattach when the old app pid is dead",
    );
}

#[cfg(target_os = "macos")]
#[test]
fn set_app_pid_repins_trust_root() {
    // After a successful reattach the engine re-pins app_pid so RPC
    // authorization (SpawnWorkerPane, BossOnly/AppOrBoss) follows the
    // live app across the restart.
    let server_state = server_state_with_app_pid(1);
    assert_eq!(server_state.current_app_pid(), Some(1));
    let self_pid = std::process::id() as libc::pid_t;
    server_state.set_app_pid(self_pid);
    assert_eq!(server_state.current_app_pid(), Some(self_pid));
    // The re-pinned pid is now a valid BossOnly trust root (the test
    // process is its own descendant), proving the auth gate reads
    // the updated value.
    assert!(server_state.authorize_rpc(RpcTier::BossOnly, Some(self_pid)));
}

#[cfg(target_os = "macos")]
#[test]
fn boss_only_rejects_worker_descendant_when_boss_pid_unregistered() {
    // Even with the boss_pid-missing fallback, anything descending
    // from a registered worker pane must still be rejected as
    // BossOnly — workers are siblings under the app and must not
    // pass live-control checks.
    let self_pid = std::process::id() as libc::pid_t;
    let server_state = server_state_with_app_pid(self_pid);
    // Mark the test process itself as a "worker" by registering its
    // pid in the WorkerRegistry. The auth check walks its own
    // ancestor chain looking for any registered worker pid; the
    // self-as-worker case hits on the first walk step.
    server_state
        .worker_registry
        .register(self_pid, "fake-run".to_owned());
    assert!(
        !server_state.authorize_rpc(RpcTier::BossOnly, Some(self_pid)),
        "BossOnly must reject callers descending from a registered worker pid",
    );
}

#[cfg(target_os = "macos")]
#[test]
fn boss_only_uses_boss_pid_when_registered() {
    let self_pid = std::process::id() as libc::pid_t;
    // Use a clearly bogus pid for app — the BossOnly path should
    // never reach the app-fallback when boss_pid is set. Setting
    // boss_pid to self_pid lets the boss-pid descendant check pass.
    let server_state = server_state_with_app_pid(1);
    server_state.set_boss_pid(self_pid);
    assert!(
        server_state.authorize_rpc(RpcTier::BossOnly, Some(self_pid)),
        "BossOnly must accept boss_pid descendants",
    );
}

#[cfg(target_os = "macos")]
#[test]
fn user_tier_admits_caller_outside_app_and_boss_subtrees() {
    // `bossctl workspace summary` is User-tier (read-only proxy of
    // `cube workspace list`). Locks in that authorize_rpc(User, …)
    // accepts a caller even when both trust roots are set and the
    // caller descends from neither — the live-coordinator-session
    // failure mode that `AppOrBoss` used to share.
    //
    // Sanity: with no workers registered, AppOrBoss now admits the
    // same caller too (the worker-exclusion fallback). The User
    // tier's value isn't its strictness — it's that it skips the
    // worker-exclusion walk entirely, so it stays correct even
    // when the caller IS a worker descendant. We exercise that
    // worker-rejection invariant in
    // `app_or_boss_rejects_worker_descendant_outside_app_subtree`.
    let server_state = server_state_with_app_pid(1);
    server_state.set_boss_pid(2);
    let self_pid = std::process::id() as libc::pid_t;
    assert!(
        server_state.authorize_rpc(RpcTier::User, Some(self_pid)),
        "User tier must accept callers outside both trust subtrees",
    );
}

#[cfg(target_os = "macos")]
#[test]
fn app_or_boss_admits_caller_outside_subtrees_when_not_a_worker() {
    // Repro for the work item: `bossctl agents transcript` (and its
    // AppOrBoss siblings — probe, stop, focus, send, interrupt,
    // cancel) was rejecting the live coordinator session because
    // the Boss session ran from a shell that descended from
    // neither the registered app pid nor the registered Boss pid.
    // The strict subtree-only gate failed and the engine returned
    // "tail_run_transcript requires app or Boss authority". The
    // fix admits any caller that isn't a registered worker
    // descendant, which covers plain terminals, tmux panes
    // pre-dating the app, separate Claude Code instances driving
    // bossctl, etc. Workers are still excluded — locked in by the
    // companion test below.
    let server_state = server_state_with_app_pid(1);
    server_state.set_boss_pid(2);
    let self_pid = std::process::id() as libc::pid_t;
    assert!(
        server_state.authorize_rpc(RpcTier::AppOrBoss, Some(self_pid)),
        "AppOrBoss must accept callers outside both trust subtrees so the live coordinator can use bossctl",
    );
}

#[cfg(target_os = "macos")]
#[test]
fn app_or_boss_rejects_worker_descendant_outside_app_subtree() {
    // Defense-in-depth for the AppOrBoss fallback: a caller that
    // is *not* under app/boss trust subtrees but IS a worker
    // descendant must still be rejected. Workers are the only
    // sibling-process adversary in the V2 threat model; the
    // worker-pid exclusion is the only thing keeping
    // `tail_run_transcript` from leaking one worker's transcript
    // into another worker's hands. The test process registers
    // itself as a worker so the ancestor walk hits on step zero.
    // app_pid is set to i32::MAX (an impossible PID on any platform)
    // so the fast-path trust-subtree check definitely fails — PID 1
    // (launchd/init) would NOT work because all processes descend from it.
    let server_state = server_state_with_app_pid(i32::MAX);
    let self_pid = std::process::id() as libc::pid_t;
    server_state
        .worker_registry
        .register(self_pid, "fake-run".to_owned());
    assert!(
        !server_state.authorize_rpc(RpcTier::AppOrBoss, Some(self_pid)),
        "AppOrBoss must reject worker descendants even when they sit outside the app/Boss subtrees",
    );
}

#[test]
fn queue_probe_mints_unique_probe_ids() {
    let server_state = test_server_state();
    let id_one = server_state.queue_probe("run-x".into(), "first".into(), false);
    let id_two = server_state.queue_probe("run-x".into(), "second".into(), false);
    assert_ne!(id_one, id_two, "probe ids must be unique per call");
    assert!(id_one.starts_with("probe-"));
    assert!(id_two.starts_with("probe-"));
    let popped_one = server_state
        .pop_pending_probe("run-x")
        .expect("first probe present");
    let popped_two = server_state
        .pop_pending_probe("run-x")
        .expect("second probe present");
    assert_eq!(popped_one.probe_id, id_one);
    assert_eq!(popped_one.text, "first");
    assert_eq!(popped_two.probe_id, id_two);
    assert_eq!(popped_two.text, "second");
    assert!(
        server_state.pop_pending_probe("run-x").is_none(),
        "queue must be empty after both probes pop",
    );
}

#[test]
fn extract_last_assistant_text_handles_modern_content_blocks() {
    let chunk = r#"{"type":"user","message":{"content":[{"type":"text","text":"prompt"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"alpha "},{"type":"text","text":"beta"}]}}
{"type":"system","subtype":"ping"}
"#;
    let result = extract_last_assistant_text(chunk);
    assert_eq!(result.as_deref(), Some("alpha beta"));
}

#[test]
fn extract_last_assistant_text_picks_most_recent_when_multiple() {
    let chunk = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"old"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"new"}]}}
"#;
    let result = extract_last_assistant_text(chunk);
    assert_eq!(result.as_deref(), Some("new"));
}

#[test]
fn extract_last_assistant_text_returns_none_when_no_assistant_turn() {
    let chunk = r#"{"type":"user","message":{"content":[{"type":"text","text":"hi"}]}}
{"type":"system","subtype":"compact"}
"#;
    assert_eq!(extract_last_assistant_text(chunk), None);
}

#[test]
fn extract_last_assistant_text_skips_unparseable_lines() {
    let chunk = "this is not json\n{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"survived\"}]}}\n";
    assert_eq!(
        extract_last_assistant_text(chunk).as_deref(),
        Some("survived"),
    );
}

#[tokio::test]
async fn dispatch_probe_reply_emits_probe_replied_after_followup_stop() {
    // End-to-end smoke for the ProbeReplied flow: call queue_probe,
    // dispatch the probe via the events-socket Stop hook, append an
    // assistant turn to the transcript, fire the follow-up Stop,
    // and observe ProbeReplied land on the per-run probe topic.
    // This locks in the wire shape a `bossctl probe --wait` (or
    // any other observer) would consume.
    use crate::protocol::WorkerEvent;
    use boss_protocol::{CreateChoreInput, CreateProductInput, RequestExecutionInput};

    let server_state = test_server_state();

    // Seed: product → chore → execution → run with a real
    // transcript path on disk. Without the run row the engine's
    // dispatch can't resolve a transcript path and would skip
    // emission — that's the production behaviour we want covered.
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
    let transcript_dir = tempfile::tempdir().unwrap();
    let transcript_path = transcript_dir.path().join("transcript.jsonl");
    std::fs::write(
        &transcript_path,
        "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n",
    )
    .unwrap();
    // Create the work_runs row so transcript_path_for_execution(execution.id)
    // can resolve the path. The run.id is not used for hook correlation — in
    // production BOSS_RUN_ID carries execution.id (exec_*), not run.id (run_*).
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

    // Map the execution (via its exec_* id) to slot 1 so dispatch_probe_on_stop
    // has a target for `SendToPane`. In production BOSS_RUN_ID carries
    // execution.id (exec_*), not run.id (run_*).
    server_state
        .worker_registry
        .register_run_slot(execution.id.clone(), 1);

    // Subscribe a session to the per-run probe topic and pin the
    // ServerState so probe pushes have somewhere to land.
    let session_id = "session-probe-observer".to_owned();
    let sink = make_session_sink();
    server_state
        .topic_broker
        .register_session(&session_id, sink.clone())
        .await;
    server_state
        .topic_broker
        .subscribe(&session_id, &[probe_topic(&execution.id)])
        .await;

    // Register a fake "app session" to receive the SendToPane that
    // dispatch_probe_on_stop emits, and reply success to it on a
    // background task. Without this round-trip the dispatch errors
    // out, the probe text gets requeued, and no in-flight entry
    // is recorded.
    let app_sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), app_sink.clone())
        .await;
    let server_for_app = server_state.clone();
    let app_responder = tokio::spawn(async move {
        let envelope = app_sink
            .next()
            .await
            .expect("SendToPane EngineRequest should be enqueued");
        let request_id = match &envelope.payload {
            FrontendEvent::EngineRequest { request_id, .. } => request_id.clone(),
            other => panic!("expected EngineRequest, got {other:?}"),
        };
        server_for_app
            .deliver_app_response(
                "session-app",
                &request_id,
                EngineToAppResponse::SendToPane {
                    result: Ok(crate::protocol::SendToPaneResult {}),
                },
            )
            .await;
    });

    // Queue a probe and pull the minted probe_id back out of the
    // queue head so we can assert it threads through to ProbeReplied.
    // In production BOSS_RUN_ID is execution.id (exec_*), so probe
    // operations use execution.id, not run.id.
    let probe_id = server_state.queue_probe(execution.id.clone(), "what now?".into(), false);

    // Fire the first Stop boundary. This dispatches the probe to
    // the (fake) app session and records the in-flight entry.
    let first_stop = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(execution.id.clone()),
        transcript_path: None,
        event: WorkerEvent::Stop {
            session_id: "claude-sess-1".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
    };
    dispatch_probe_reply_on_stop(&server_state, &first_stop).await;
    dispatch_probe_on_stop(&server_state, &first_stop).await;
    app_responder.await.expect("app responder task");

    // Append an assistant turn — the worker has now "replied".
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript_path)
            .unwrap();
        writeln!(
            file,
            "{}",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"the answer\"}]}}",
        )
        .unwrap();
    }

    // Second Stop: the engine should see the in-flight probe,
    // read the new transcript bytes, and publish ProbeReplied on
    // the per-run probe topic.
    let second_stop = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(execution.id.clone()),
        transcript_path: None,
        event: WorkerEvent::Stop {
            session_id: "claude-sess-1".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
    };
    dispatch_probe_reply_on_stop(&server_state, &second_stop).await;

    let envelope = sink
        .next()
        .await
        .expect("ProbeReplied envelope should be published");
    match envelope.payload {
        FrontendEvent::ProbeReplied {
            run_id: emitted_run,
            probe_id: emitted_probe,
            text,
        } => {
            assert_eq!(emitted_run, execution.id);
            assert_eq!(emitted_probe, probe_id);
            assert_eq!(text, "the answer");
        }
        other => panic!("expected ProbeReplied, got {other:?}"),
    }

    // Idempotency: a duplicate Stop with no in-flight entry must
    // not re-emit the same probe id.
    dispatch_probe_reply_on_stop(&server_state, &second_stop).await;
    let drain = tokio::time::timeout(Duration::from_millis(50), sink.next()).await;
    assert!(
        drain.is_err(),
        "duplicate Stop must not produce a second ProbeReplied for the same probe id",
    );
}

/// Regression: `dispatch_probe_if_idle` must deliver a probe
/// immediately to a worker whose activity is `Idle` — i.e. one that
/// is between turns and has no Stop boundary coming. Before the fix,
/// `bossctl probe` targeted at an idle worker would stall forever
/// because `dispatch_probe_on_stop` only fires on Stop events and an
/// idle worker never produces another Stop without receiving input
/// first.
#[tokio::test]
async fn probe_queued_for_idle_worker_dispatches_immediately() {
    use boss_protocol::{
        CreateChoreInput, CreateProductInput, RequestExecutionInput, WorkerActivity,
        WorkerEvent,
    };

    let server_state = test_server_state();

    // Minimal DB rows so transcript lookup has something to resolve.
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

    // Register slot and set activity to Idle (worker between turns).
    server_state
        .worker_registry
        .register_run_slot(run.id.clone(), 1);
    server_state.live_worker_states.register_spawn(
        1,
        run.id.clone(),
        "claude-opus-4-7",
        0,
        None,
    );
    // Apply a Stop event to transition Spawning → Idle.
    server_state.live_worker_states.apply_event(
        1,
        &WorkerEvent::Stop {
            session_id: "sess-1".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
    );
    assert_eq!(
        server_state.live_worker_states.get(1).unwrap().activity,
        WorkerActivity::Idle,
        "precondition: worker must be idle",
    );

    // Register a fake app session to receive the SendToPane.
    let app_sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), app_sink.clone())
        .await;
    let server_for_app = server_state.clone();
    let app_responder = tokio::spawn(async move {
        let envelope = app_sink
            .next()
            .await
            .expect("SendToPane must arrive for idle worker");
        let request_id = match &envelope.payload {
            FrontendEvent::EngineRequest { request_id, .. } => request_id.clone(),
            other => panic!("expected EngineRequest, got {other:?}"),
        };
        server_for_app
            .deliver_app_response(
                "session-app",
                &request_id,
                EngineToAppResponse::SendToPane {
                    result: Ok(crate::protocol::SendToPaneResult {}),
                },
            )
            .await;
    });

    // Queue the probe and call dispatch_probe_if_idle directly.
    server_state.queue_probe(run.id.clone(), "coordinator nudge".into(), false);
    dispatch_probe_if_idle(&server_state, &run.id).await;

    // The app_responder task must have seen the SendToPane by now.
    tokio::time::timeout(Duration::from_secs(2), app_responder)
        .await
        .expect("timed out waiting for SendToPane round-trip")
        .expect("app_responder panicked");

    // Probe must have been consumed (popped from pending_probes and
    // an in-flight entry recorded).
    assert!(
        server_state.pop_pending_probe(&run.id).is_none(),
        "probe must be consumed, not left in pending_probes",
    );
}

/// Regression: probes queued by the completion handler during a Stop
/// event must be dispatched on the SAME Stop, not stalled until the
/// next one. The event-loop order change (completion before probe
/// dispatch) enables this: `dispatch_completion_on_stop` adds to
/// `pending_probes`, then `dispatch_probe_on_stop` picks them up.
#[tokio::test]
async fn completion_probe_dispatched_on_same_stop_as_completion() {
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

    server_state
        .worker_registry
        .register_run_slot(run.id.clone(), 1);

    // Queue a probe manually (simulating what the completion handler does)
    // BEFORE dispatch_probe_on_stop fires, to verify the dispatch picks it up.
    server_state.queue_probe(run.id.clone(), "push your PR".into(), false);

    // Register a fake app session to capture the SendToPane.
    let app_sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), app_sink.clone())
        .await;
    let server_for_app = server_state.clone();
    let app_responder = tokio::spawn(async move {
        let envelope = app_sink
            .next()
            .await
            .expect("SendToPane must arrive on the same Stop that completion queued it");
        let request_id = match &envelope.payload {
            FrontendEvent::EngineRequest { request_id, .. } => request_id.clone(),
            other => panic!("expected EngineRequest, got {other:?}"),
        };
        server_for_app
            .deliver_app_response(
                "session-app",
                &request_id,
                EngineToAppResponse::SendToPane {
                    result: Ok(crate::protocol::SendToPaneResult {}),
                },
            )
            .await;
    });

    // Fire the Stop event. With the new ordering, dispatch_probe_on_stop
    // runs after dispatch_completion_on_stop and sees the queued probe.
    let stop = crate::events_socket::IncomingHookEvent {
        peer_pid: None,
        run_id: Some(run.id.clone()),
        transcript_path: None,
        event: WorkerEvent::Stop {
            session_id: "sess-1".into(),
            stop_hook_active: false,
            stop_reason: crate::protocol::StopReason::Completed,
        },
    };
    dispatch_probe_on_stop(&server_state, &stop).await;
    tokio::time::timeout(Duration::from_secs(2), app_responder)
        .await
        .expect("timed out waiting for SendToPane from completion probe")
        .expect("app_responder panicked");

    assert!(
        server_state.pop_pending_probe(&run.id).is_none(),
        "probe must be consumed by dispatch_probe_on_stop",
    );
}
