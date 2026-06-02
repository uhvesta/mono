//! `FrontendRequest` handlers — executions, runs, and transcripts.
//!
//! Split out of `app.rs`; each handler is dispatched from the
//! `handle_frontend_connection` match. Pure structural move — no
//! behavioural change. See [`super::Dispatch`] for the per-request
//! context every handler receives.

use super::*;

/// Byte cap for an over-SSH remote transcript tail pull. The
/// `TailRunTranscript` RPC requests a line count, not a byte count, so
/// we pull a generous suffix and split it to the requested lines; 256 KiB
/// comfortably covers the few-hundred-line tails the viewer requests
/// while keeping the SSH round-trip cheap. Matches the bound
/// `transient_recovery` uses for its local tail read.
const REMOTE_TRANSCRIPT_TAIL_BYTES: u64 = 256 * 1024;

pub(super) async fn handle_create_execution(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateExecution { input } = req else {
        unreachable!()
    };
    match work_db.create_execution(input) {
        Ok(execution) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::ExecutionCreated { execution },
            );
        }
        Err(err) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: err.to_string(),
                },
            );
        }
    }
}

pub(super) async fn handle_request_execution(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::RequestExecution { input } = req else {
        unreachable!()
    };
    {
        // Live-worker awareness: when the work item already has
        // a non-terminal execution, the engine reuses it only
        // when the slot registry actually still has a live
        // worker for that run id. Without this check, a chore
        // whose previous worker died with the app gets stuck
        // — the kanban drag fires RequestExecution, the engine
        // says "still running," coordinator polls for `ready`
        // and sees nothing, no new spawn ever happens.
        //
        // `force = true` is the `bossctl agents launch`
        // entry point: same DB row creation, but we hand the
        // ready execution straight to
        // `ExecutionCoordinator::force_dispatch` instead of
        // kicking the auto-dispatcher. force_dispatch grows
        // the worker pool by one slot (bounded by the hard
        // cap) when every configured slot is busy, so the
        // launch verb skips the cap-deferral the normal
        // request path would otherwise hit.
        let force = input.force;
        let live_states = server_state.live_worker_states.clone();
        let result = work_db
            .request_execution_with_live_check(input, |run_id| live_states.is_run_live(run_id));
        match result {
            Ok(execution) => {
                if force {
                    // If the request landed on an existing
                    // non-terminal execution (idempotent path
                    // when a live worker already runs the
                    // item), just refresh the row and skip
                    // force-dispatch — there's no second
                    // worker to spawn.
                    if execution.status == "ready" {
                        let coordinator = server_state.execution_coordinator.clone();
                        let execution_id = execution.id.clone();
                        match coordinator.force_dispatch(&execution_id).await {
                            Ok(_worker_id) => {}
                            Err(err) => {
                                send_response(
                                    &sink,
                                    &request_id,
                                    FrontendEvent::WorkError {
                                        message: err.to_string(),
                                    },
                                );
                                return;
                            }
                        }
                    }
                    // Re-read the execution after force_dispatch
                    // so the response carries the row's now-
                    // running status (and worker / lease ids).
                    let refreshed = match work_db.get_execution(&execution.id) {
                        Ok(execution) => execution,
                        Err(_) => execution,
                    };
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::ExecutionRequested {
                            execution: refreshed,
                        },
                    );
                } else {
                    // Log every queued request so an operator can pair
                    // a `bossctl work start` call with the engine-side
                    // outcome even when the scheduler races the row
                    // (the kick-noop/lost-wakeup class of bug). The
                    // structured `spawn_attempt` line lands in
                    // `run_scheduler` once it picks the row up; this
                    // line bookends the request itself.
                    tracing::info!(
                        execution_id = %execution.id,
                        work_item_id = %execution.work_item_id,
                        execution_status = %execution.status,
                        "RequestExecution accepted -> kicking scheduler"
                    );
                    server_state.execution_coordinator.kick();
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::ExecutionRequested { execution },
                    );
                }
            }
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: err.to_string(),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_list_executions(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListExecutions { work_item_id } = req else {
        unreachable!()
    };
    {
        match work_db.list_executions(work_item_id.as_deref()) {
            Ok(executions) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::ExecutionsList {
                        work_item_id,
                        executions,
                    },
                );
            }
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: err.to_string(),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_get_task_runtime(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetTaskRuntime { work_item_id } = req else {
        unreachable!()
    };
    {
        match work_db.get_task_runtime(&work_item_id) {
            Ok(runtime) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::TaskRuntimeResult { runtime },
                );
            }
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: err.to_string(),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_get_execution(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetExecution { id } = req else {
        unreachable!()
    };
    match work_db.get_execution(&id) {
        Ok(execution) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::ExecutionResult { execution },
            );
        }
        Err(err) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: err.to_string(),
                },
            );
        }
    }
}

pub(super) async fn handle_create_run(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::CreateRun { input } = req else {
        unreachable!()
    };
    match work_db.create_run(input) {
        Ok(run) => {
            send_response(&sink, &request_id, FrontendEvent::RunCreated { run });
        }
        Err(err) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: err.to_string(),
                },
            );
        }
    }
}

pub(super) async fn handle_list_runs(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListRuns { execution_id } = req else {
        unreachable!()
    };
    match work_db.list_runs(&execution_id) {
        Ok(runs) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::RunsList { execution_id, runs },
            );
        }
        Err(err) => {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: err.to_string(),
                },
            );
        }
    }
}

pub(super) async fn handle_get_run(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::GetRun { id } = req else {
        unreachable!()
    };
    {
        // Try the run_* namespace first, then fall back to the
        // exec_* namespace. Callers such as `bossctl agents
        // status` pass whatever id they have in hand — often an
        // execution id (exec_*) — but `get_run` joins against
        // `work_runs.id` (run_*), so the lookup silently fails
        // with "unknown run". `list_runs(exec_id)` finds the
        // run via `work_runs.execution_id` and returns the most
        // recent one (the active or last-completed run for that
        // execution).
        let result = work_db
            .get_run(&id)
            .ok()
            .or_else(|| work_db.list_runs(&id).ok().and_then(|mut runs| runs.pop()));
        match result {
            Some(run) => {
                send_response(&sink, &request_id, FrontendEvent::RunResult { run });
            }
            None => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("unknown run: {id}"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_probe_run(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::ProbeRun {
        run_id,
        text,
        urgent,
    } = req
    else {
        unreachable!()
    };
    {
        // `bossctl probe` is a coordinator-essential verb (the
        // coordinator contract names probing as the right tool
        // for low-confidence handoffs). The earlier BossOnly
        // gate rejected calls from worker (slot) panes, since
        // BossOnly explicitly excludes callers descending from
        // a registered worker shell pid. Same reasoning as the
        // `stop_run` fix in PR #218: downgrade to AppOrBoss so
        // any caller descending from the app or the Boss
        // session is accepted, including worker panes.
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                run_id = %run_id,
                "probe_run rejected: caller not in app/Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "probe_run requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        let probe_id = server_state.queue_probe(run_id.clone(), text, urgent);
        tracing::info!(run_id = %run_id, probe_id = %probe_id, urgent, "probe queued");
        // Immediately deliver the probe if the worker is already idle
        // (between turns). An idle worker has no Stop boundary coming
        // — `dispatch_probe_on_stop` would never fire — so we push the
        // text into the pane right now. If the worker is active the
        // call is a no-op and the probe waits for the next Stop.
        let server_for_idle = server_state.clone();
        let run_id_for_idle = run_id.clone();
        tokio::spawn(async move {
            dispatch_probe_if_idle(&server_for_idle, &run_id_for_idle).await;
        });
        send_response(
            &sink,
            &request_id,
            FrontendEvent::ProbeQueued {
                run_id,
                probe_id,
                urgent,
            },
        );
    }
}

pub(super) async fn handle_stop_run(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::StopRun { run_id } = req else {
        unreachable!()
    };
    {
        // `bossctl agents stop` is the coordinator superset's
        // imperative kill switch, and the human invokes it
        // from wherever they happen to be — including the
        // boss pane, the macOS app shell, or *inside a worker
        // pane* (e.g. tab over to slot 1, type `bossctl
        // agents stop <id>`). The earlier BossOnly gate
        // rejected the worker-pane case because callers
        // descending from a registered worker shell pid are
        // explicitly excluded from BossOnly. Downgrade to
        // AppOrBoss to match `cancel_execution`: any caller
        // descending from the app or the Boss session is
        // accepted, which covers worker panes too.
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                run_id = %run_id,
                "stop_run rejected: caller not in app/Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "stop_run requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        tracing::info!(run_id = %run_id, "stop_run requested");
        let handler = server_state.completion_handler.clone();
        let run_id_for_release = run_id.clone();
        tokio::spawn(async move {
            // Use `force_stop_execution` instead of plain
            // `force_release`: this additionally cancels the
            // execution row and demotes the task from `active`
            // back to `todo` so the orphan sweep and
            // `reconcile_active_dispatch` cannot immediately
            // re-dispatch the work item the moment the worker
            // pool slot is freed.
            handler.force_stop_execution(&run_id_for_release).await;
        });
        send_response(&sink, &request_id, FrontendEvent::RunStopped { run_id });
    }
}

pub(super) async fn handle_cancel_execution(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::CancelExecution { execution_id } = req else {
        unreachable!()
    };
    {
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                execution_id = %execution_id,
                "cancel_execution rejected: caller not in app/Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "cancel_execution requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        match server_state.work_db.cancel_execution(&execution_id) {
            Ok(execution) => {
                tracing::info!(
                    execution_id = %execution_id,
                    "cancel_execution: marked cancelled",
                );
                // Pane releases are keyed by run_id (the slot
                // registry's key), not by execution_id — so
                // walk the execution's still-active runs and
                // release each. Idempotent on the registry side.
                let active_runs = server_state
                    .work_db
                    .active_run_ids_for_execution(&execution_id)
                    .unwrap_or_default();
                let handler = server_state.completion_handler.clone();
                let exec_for_release = execution_id.clone();
                tokio::spawn(async move {
                    for run_id in active_runs {
                        handler.force_release(&run_id).await;
                    }
                    // Final pass keyed by execution_id so the
                    // cube workspace lease (which is recorded
                    // on the execution row) is released even
                    // when the execution had no active run.
                    handler.force_release(&exec_for_release).await;
                });
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::ExecutionCancelled { execution },
                );
            }
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: err.to_string(),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_reap_run(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::ReapRun { run_id } = req else {
        unreachable!()
    };
    {
        // `bossctl agents reap` is the manual escape hatch for
        // orphans the engine startup probe missed (e.g. the
        // cube lease was still within its TTL on relaunch, so
        // the probe said "Live" even though the libghostty
        // pane is gone). Gate it `BossOnly`: this is a state
        // mutation that should not be reachable from a worker
        // pane subtree.
        if !server_state.authorize_rpc(RpcTier::BossOnly, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                run_id = %run_id,
                "reap_run rejected: caller not in Boss subtree",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "reap_run requires Boss authority".to_owned(),
                },
            );
            return;
        }
        let reason = "manual reap via bossctl agents reap";
        match server_state
            .work_db
            .mark_execution_orphaned(&run_id, reason)
        {
            Ok(execution) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    cube_workspace_id = ?execution.cube_workspace_id,
                    "reap_run: marked execution orphaned (workspace preserved)",
                );
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::RunReaped { run_id, execution },
                );
            }
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: err.to_string(),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_tail_run_transcript(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::TailRunTranscript { run_id, lines } = req else {
        unreachable!()
    };
    {
        // `bossctl agents transcript` is a documented
        // coordinator verb. The earlier strict subtree-only
        // AppOrBoss check rejected the live coordinator when
        // it ran from a shell that descended from neither the
        // app nor the registered Boss session — see the
        // `authorize_rpc` AppOrBoss docstring for the
        // worker-exclusion fallback that fixed it. We still
        // reject worker descendants so one worker can't
        // tail another worker's transcript.
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            tracing::warn!(
                peer_pid = ?peer_pid,
                run_id = %run_id,
                "tail_run_transcript rejected: caller is a worker descendant",
            );
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "tail_run_transcript requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        match resolve_transcript_for_tail(&server_state, &run_id) {
            TranscriptResolution::Found { transcript_path } => {
                // The recorded `transcript_path` is interpreted on the
                // host that produced it. For a local run it is a path on
                // the engine's own filesystem; for a remote run it lives
                // on the remote host and must be pulled over SSH. Resolve
                // the run's host (the id may be a `run_*` or `exec_*`
                // namespace, so try both keys) and route accordingly so
                // `bossctl agents transcript` / the transcript viewer work
                // identically against a remote worker.
                let host = server_state
                    .work_db
                    .run_host(&run_id)
                    .ok()
                    .flatten()
                    .or_else(|| {
                        server_state
                            .work_db
                            .latest_run_host_for_execution(&run_id)
                            .ok()
                            .flatten()
                    })
                    .unwrap_or_else(|| "local".to_owned());
                let read_result: Result<(Vec<String>, bool), String> = if host == "local" {
                    read_transcript_tail(&transcript_path, lines)
                        .await
                        .map_err(|err| format!("transcript read failed for {transcript_path}: {err}"))
                } else {
                    match server_state
                        .execution_coordinator
                        .read_remote_transcript_tail(
                            &host,
                            &transcript_path,
                            REMOTE_TRANSCRIPT_TAIL_BYTES,
                        )
                        .await
                    {
                        Ok(Some(content)) => Ok(tail_lines_from_content(&content, lines)),
                        // A remote host reporting `None` means "treat as
                        // local"; fall back to the local read rather than
                        // surface a spurious error.
                        Ok(None) => read_transcript_tail(&transcript_path, lines)
                            .await
                            .map_err(|err| {
                                format!("transcript read failed for {transcript_path}: {err}")
                            }),
                        Err(err) => Err(format!(
                            "remote transcript read failed for {transcript_path} on host {host}: {err:#}"
                        )),
                    }
                };
                match read_result {
                    Ok((lines_out, truncated)) => {
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::RunTranscriptTail {
                                run_id,
                                transcript_path,
                                lines: lines_out,
                                truncated,
                            },
                        );
                    }
                    Err(message) => {
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError { message },
                        );
                    }
                }
            }
            TranscriptResolution::Buffering => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!(
                            "{TRANSCRIPT_NOT_YET_AVAILABLE_PREFIX}{run_id}: engine has not yet received a hook event carrying transcript_path (retry in a few seconds)"
                        ),
                    },
                );
            }
            TranscriptResolution::KnownNoTranscript => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("run {run_id} has no transcript path recorded"),
                    },
                );
            }
            TranscriptResolution::Unknown => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("unknown run: {run_id}"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_execution_transcript(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        server_state,
        work_db,
        sink,
        request_id,
        peer_pid,
        ..
    } = ctx;
    let FrontendRequest::ExecutionTranscript { execution_id } = req else {
        unreachable!()
    };
    {
        if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
            send_response(
                &sink,
                &request_id,
                FrontendEvent::Error {
                    message: "execution_transcript requires app or Boss authority".to_owned(),
                },
            );
            return;
        }
        let execution = match work_db.get_execution(&execution_id) {
            Ok(exec) => exec,
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: err.to_string(),
                    },
                );
                return;
            }
        };
        let is_live = execution.finished_at.is_none()
            && matches!(execution.status.as_str(), "running" | "waiting_human");
        let transcript_path = match work_db.transcript_path_for_execution(&execution_id) {
            Ok(Some(path)) => path,
            Ok(None) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::ExecutionTranscriptUnavailable {
                        execution_id,
                        reason: "no transcript path recorded for this execution".to_owned(),
                    },
                );
                return;
            }
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("transcript lookup failed for {execution_id}: {err}"),
                    },
                );
                return;
            }
        };
        match tokio::fs::read_to_string(&transcript_path).await {
            Ok(content) => {
                let events = crate::transcript_markdown::parse_transcript(&content);
                let segments =
                    crate::transcript_markdown::events_to_segments(&events, &Default::default());
                let wire_segments: Vec<boss_protocol::TranscriptSegment> =
                    segments.into_iter().map(segment_to_wire).collect();
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::ExecutionTranscriptResult {
                        execution_id,
                        segments: wire_segments,
                        is_live,
                        complete: !is_live,
                    },
                );
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::ExecutionTranscriptUnavailable {
                        execution_id,
                        reason: format!("transcript file not found: {transcript_path}"),
                    },
                );
            }
            Err(err) => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: format!("transcript read failed for {transcript_path}: {err}"),
                    },
                );
            }
        }
    }
}

pub(super) async fn handle_list_engine_attempts(ctx: Dispatch, req: FrontendRequest) {
    let Dispatch {
        work_db,
        sink,
        request_id,
        ..
    } = ctx;
    let FrontendRequest::ListEngineAttempts {
        kinds,
        product_id,
        status,
        work_item_id,
        limit,
    } = req
    else {
        unreachable!()
    };
    {
        match work_db.list_engine_attempts(
            &kinds,
            product_id.as_deref(),
            &status,
            work_item_id.as_deref(),
            limit,
        ) {
            Ok(attempts) => send_response(
                &sink,
                &request_id,
                FrontendEvent::EngineAttemptsList { attempts },
            ),
            Err(err) => send_response(
                &sink,
                &request_id,
                FrontendEvent::WorkError {
                    message: err.to_string(),
                },
            ),
        }
    }
}
