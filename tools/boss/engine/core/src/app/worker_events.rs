//! Worker hook-event dispatch functions.
//!
//! Split out of `app.rs`; all `dispatch_*` functions that react to worker
//! hook events live here. Pure structural move — no behavioural change.

use super::*;

/// Update the per-slot LiveWorkerState for the run this hook event
/// belongs to and push the new snapshot on the
/// `worker.live_states` topic if anything changed. Hook events that
/// arrive before the run has been registered (e.g., the spawn flow
/// hasn't recorded the slot yet) are silently dropped — once the
/// registration lands, subsequent events will hit the live entry.
fn worker_event_kind(event: &crate::protocol::WorkerEvent) -> &'static str {
    use crate::protocol::WorkerEvent;
    match event {
        WorkerEvent::SessionStart { .. } => "session_start",
        WorkerEvent::UserPromptSubmit { .. } => "user_prompt_submit",
        WorkerEvent::PreToolUse { .. } => "pre_tool_use",
        WorkerEvent::PostToolUse { .. } => "post_tool_use",
        WorkerEvent::Stop { .. } => "stop",
        WorkerEvent::Notification { .. } => "notification",
        WorkerEvent::SessionEnd { .. } => "session_end",
    }
}

pub(super) async fn dispatch_live_worker_state(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
) {
    let event_kind = worker_event_kind(&incoming.event);
    server_state.dispatcher_stats.inc_hook_events_total();
    tracing::info!(
        run_id = ?incoming.run_id,
        peer_pid = ?incoming.peer_pid,
        kind = event_kind,
        has_transcript_path = incoming.transcript_path.is_some(),
        "live_status: hook payload arrived at dispatcher",
    );
    let Some(run_id) = incoming.run_id.as_deref() else {
        server_state.dispatcher_stats.inc_dropped_missing_run_id();
        tracing::warn!(
            kind = event_kind,
            peer_pid = ?incoming.peer_pid,
            "live_status: dropping hook — neither _boss_run_id payload nor peer-pid ancestor walk produced a run_id",
        );
        return;
    };
    server_state
        .dispatcher_stats
        .record_last_hook(run_id, event_kind);
    // Persist the transcript path the moment we see it on a hook
    // payload. `start_execution_run` inserts the work_runs row with
    // `transcript_path = NULL` (the engine has no way to know the
    // path until the worker tells us via its first hook), so without
    // this write the live-status summarizer's `TranscriptPathResolver`
    // returns None forever and the per-slot loop early-outs every
    // tick on "no transcript path yet". The setter is idempotent
    // (first-writer-wins) so we don't clobber the path the tail
    // watcher has already opened across later sessions/resumes.
    //
    // This runs BEFORE the slot lookup so it survives the cases where
    // `slot_for_run` would otherwise drop the event: a first hook
    // racing ahead of `register_run_slot`, an engine restart that
    // wipes the in-memory `WorkerRegistry` while pre-existing workers
    // keep firing hooks, or a late hook arriving after the slot has
    // been released. The persist is keyed solely on `run_id` and does
    // not need the slot mapping — gating it under that lookup was the
    // gap that pinned `work_runs.transcript_path` at NULL across
    // engine restarts.
    //
    // **2026-05-12 follow-up:** PR #366's persist branch only fires
    // when the current hook's payload carries `transcript_path`. In
    // production that turned out to be insufficient — claude does
    // not include the field on every event kind, and the work_runs
    // row may not even exist yet at the moment a SessionStart fires
    // (the engine inserts it from a separate code path that races
    // the worker's startup hooks). The fix is to cache the path
    // engine-side keyed by run id, so a later PostToolUse / Stop /
    // whatever can persist the cached value even when its own
    // payload omits the field.
    let payload_path = incoming.transcript_path.as_deref();
    let (resolved_path, from_cache) = match payload_path {
        Some(path) => {
            server_state.dispatcher_stats.inc_with_transcript_path();
            let _ = server_state
                .transcript_path_cache
                .record_if_unset(run_id, path);
            (Some(path.to_owned()), false)
        }
        None => {
            server_state.dispatcher_stats.inc_without_transcript_path();
            match server_state.transcript_path_cache.get(run_id) {
                Some(cached) => (Some(cached), true),
                None => (None, false),
            }
        }
    };
    if let Some(path) = resolved_path.as_deref() {
        // `run_id` here is the `_boss_run_id` from the hook payload,
        // which carries the **execution_id** (`exec_*`) — not a
        // `work_runs.id` (`run_*`). The setter joins on
        // `work_runs.execution_id` so the caller doesn't have to
        // care; the local `execution_id` binding is just to make
        // the namespace explicit at the call site, since the
        // historical "run_id" naming all the way through the
        // dispatcher is what hid the wrong-namespace bug.
        let execution_id = run_id;
        match server_state
            .work_db
            .set_run_transcript_path_if_unset(execution_id, path)
        {
            Ok(SetRunTranscriptPathOutcome::Updated) => {
                server_state.dispatcher_stats.inc_persist_updated();
                if from_cache {
                    server_state.dispatcher_stats.inc_persist_from_cache();
                }
                tracing::info!(
                    execution_id,
                    transcript_path = %path,
                    from_cache,
                    "recorded transcript_path on work_run from hook payload",
                );
            }
            Ok(SetRunTranscriptPathOutcome::AlreadySet) => {
                server_state.dispatcher_stats.inc_persist_noop();
            }
            Ok(SetRunTranscriptPathOutcome::RowMissing) => {
                server_state.dispatcher_stats.inc_persist_row_missing();
                tracing::warn!(
                    execution_id,
                    transcript_path = %path,
                    "no work_runs row for execution yet; transcript_path persist deferred to a later hook",
                );
            }
            Err(err) => {
                server_state.dispatcher_stats.inc_persist_err();
                tracing::warn!(
                    execution_id,
                    ?err,
                    "failed to persist transcript_path from hook payload",
                );
            }
        }
    }
    let slot_id = match server_state.worker_registry.slot_for_run(run_id) {
        Some(slot_id) => slot_id,
        None => {
            // No slot mapping. A *remote* worker never gets a libghostty
            // pane (it holds no local slot), so the spawn flow never
            // called `register_run_slot` for it — yet its hooks tunnel
            // back here over the forwarded events socket. Lazily assign a
            // virtual slot so the live-status surface tracks the remote
            // worker's activity (Spawning/Working/Idle/…) just like a
            // local one. This is also how a worker reattached after an
            // engine restart re-acquires its live-status slot: the first
            // hook over the re-established forward lands here. Local runs
            // with no slot are genuinely gone or racing ahead of
            // registration (the historical drop case) and fall through.
            match register_remote_worker_slot(server_state, run_id).await {
                Some(slot_id) => slot_id,
                None => {
                    tracing::warn!(
                        run_id,
                        kind = event_kind,
                        "live_status: dropping hook fan-out — run_id is not registered against a slot (event ahead of register_run_slot or after take_slot_for_run, or a non-remote run); transcript_path already persisted",
                    );
                    return;
                }
            }
        }
    };
    // Remote workers get a virtual slot but no per-slot live-status
    // summarizer task (the AI-summary loop tails a *local* transcript
    // file, which a remote run does not have — wiring it to the
    // over-SSH pull is a documented follow-up). The activity surface
    // (`apply_event` + broadcast below) is what drives the live dot and
    // works for remote runs; only the summarizer-trigger `notify` calls
    // are gated off so they don't emit a misleading "notify dropped — no
    // per-slot task" warn on every remote hook.
    let is_remote_slot = slot_id >= crate::worker_registry::REMOTE_SLOT_BASE;
    let prior_activity = server_state
        .live_worker_states
        .get(slot_id)
        .map(|s| s.activity);
    let changed = server_state
        .live_worker_states
        .apply_event(slot_id, &incoming.event);
    if changed {
        server_state.broadcast_live_worker_states().await;
    }
    // Fan out the matching trigger to the per-slot live-status loop.
    // The manager drops the trigger if no slot task is running, so a
    // hook arriving before `register_spawn` or after `release_slot`
    // is a benign no-op.
    let new_activity = server_state
        .live_worker_states
        .get(slot_id)
        .map(|s| s.activity);
    match &incoming.event {
        crate::protocol::WorkerEvent::Stop { .. } => {
            if !is_remote_slot {
                server_state
                    .live_status_manager
                    .notify(slot_id, Trigger::Stop);
            }
        }
        crate::protocol::WorkerEvent::PostToolUse {
            tool_name,
            tool_input,
            tool_response,
            ..
        } => {
            if !is_remote_slot {
                server_state
                    .live_status_manager
                    .notify(slot_id, Trigger::PostToolUse);
            }
            // Primary-path PR URL capture. Every worker that opens a
            // PR does it via a Bash `gh pr create` (and also
            // `gh pr view` / `gh pr edit`); the PR URL is printed
            // on stdout. Catch it here, stage against the
            // execution_id, and the on-Stop handler picks it up
            // without ever shelling out to `jj log` to reconstruct
            // it.
            //
            // Layer-1 gate: only capture URLs from deliberate `gh pr`
            // invocations. Arbitrary Bash output (file reads, test
            // runs, chore descriptions printed via shell) can contain
            // PR URLs from unrelated executions; filtering by command
            // prevents those from staging the wrong PR.
            if tool_name == "Bash" {
                // Check for any PR URL first so we can log a rejection
                // when the command isn't a gh pr invocation.
                if let Some(pr_url) =
                    crate::pr_url_capture::extract_pr_url_from_bash_response(tool_response)
                {
                    if !crate::pr_url_capture::is_gh_pr_command(tool_input) {
                        tracing::info!(
                            execution_id = run_id,
                            rejected_url = %pr_url,
                            reason = "not_a_gh_pr_command",
                            "pr_url_capture_rejected: URL in Bash stdout rejected — command is not a gh pr invocation",
                        );
                    } else {
                        // Gate the URL against the product's repo before
                        // staging. Workers running tests can emit fixture
                        // URLs (e.g. `https://github.com/foo/bar/pull/42`)
                        // in tool_response.stdout; without this gate those
                        // bind to the work_item as if they were real PRs.
                        let execution_id = run_id;
                        let repo_url_result = server_state
                            .work_db
                            .get_execution(execution_id)
                            .map(|e| e.repo_remote_url);
                        let valid = match repo_url_result {
                            Ok(ref repo_url) => {
                                match crate::pr_url_capture::validate_pr_url(&pr_url, repo_url) {
                                    Ok(()) => true,
                                    Err(reason) => {
                                        tracing::info!(
                                            execution_id,
                                            rejected_url = %pr_url,
                                            %reason,
                                            "pr_url_capture: dropping URL — failed product-repo gate",
                                        );
                                        false
                                    }
                                }
                            }
                            Err(err) => {
                                tracing::warn!(
                                    execution_id,
                                    rejected_url = %pr_url,
                                    ?err,
                                    "pr_url_capture: could not load execution to validate URL; dropping for safety",
                                );
                                false
                            }
                        };
                        if valid {
                            let outcome =
                                server_state.staged_pr_urls.record_if_unset(run_id, &pr_url);
                            match outcome {
                                crate::pr_url_capture::StagePrUrlOutcome::Staged => {
                                    tracing::info!(
                                        execution_id = run_id,
                                        pr_url = %pr_url,
                                        "pr_url_capture: staged PR URL from worker hook stream",
                                    );
                                }
                                crate::pr_url_capture::StagePrUrlOutcome::AlreadyStaged => {
                                    // Worker emitted another PR URL after
                                    // already staging one — typically a
                                    // `gh pr view` follow-up referencing a
                                    // different PR. First-writer-wins so
                                    // the original (the worker's own
                                    // `gh pr create`) is kept.
                                    tracing::debug!(
                                        execution_id = run_id,
                                        pr_url = %pr_url,
                                        "pr_url_capture: ignoring later URL (already staged for this execution)",
                                    );
                                }
                            }
                        }
                    } // else (is_gh_pr_command)
                }
            }
        }
        _ => {}
    }
    if !is_remote_slot {
        if let (Some(prior), Some(new)) = (prior_activity, new_activity) {
            if prior != new {
                server_state
                    .live_status_manager
                    .notify(slot_id, Trigger::ActivityChanged(new));
            }
        } else if let Some(new) = new_activity {
            // First event lands on a freshly spawned slot — the trigger
            // gives the loop the activity it should base its initial
            // policy on (in particular, Working → starts the timer
            // floor).
            server_state
                .live_status_manager
                .notify(slot_id, Trigger::ActivityChanged(new));
        }
    }
}

/// Assign a virtual live-status slot to a slotless run when it is a
/// live **remote** worker, returning the slot to fan the hook out to.
///
/// `run_id` is the worker's `BOSS_RUN_ID`, which is the execution id.
/// A remote worker holds no libghostty pane, so the local spawn flow
/// never registered a slot for it — but the live-status surface is
/// slot-keyed, so we allocate a synthetic slot from the reserved remote
/// range (see [`crate::worker_registry::REMOTE_SLOT_BASE`]) and seed the
/// initial `LiveWorkerState` the first time we see the run. Returns
/// `None` (so the caller drops the event) when the run is not a live
/// remote worker: a local run, a run with no recorded host, a run on a
/// settled execution (late/duplicate hook for a finished worker), or
/// when the remote slot range is exhausted.
async fn register_remote_worker_slot(
    server_state: &Arc<ServerState>,
    run_id: &str,
) -> Option<u8> {
    let host = server_state
        .work_db
        .latest_run_host_for_execution(run_id)
        .ok()
        .flatten()?;
    if host == "local" {
        return None;
    }
    // Don't resurrect a finished run from a late or duplicate hook.
    let execution = server_state.work_db.get_execution(run_id).ok()?;
    if remote_execution_is_settled(&execution.status) {
        return None;
    }
    let (slot_id, freshly_allocated) = server_state
        .worker_registry
        .get_or_allocate_remote_slot(run_id)?;
    if freshly_allocated {
        // Resolve the work item once for both the binding (name) and
        // the model label. `model_override` is the user's explicit
        // choice when set; otherwise fall back to a generic label (the
        // effort-resolved model lives in the spawn-time config, which is
        // not persisted, so it is not recoverable here).
        let work_item = server_state.work_db.get_work_item(&execution.work_item_id).ok();
        let binding = work_item.as_ref().map(|item| boss_protocol::WorkItemBinding {
            work_item_id: execution.work_item_id.clone(),
            work_item_name: crate::runner::work_item_name(item).to_owned(),
            execution_id: run_id.to_owned(),
        });
        let model = work_item
            .as_ref()
            .and_then(remote_worker_model_override)
            .unwrap_or_else(|| "claude".to_owned());
        // shell_pid is a local-process concept; a remote worker has no
        // local pid, so 0 (the live state stores it but the value is
        // only meaningful for the local ancestor-walk correlation that
        // remote runs bypass via the `_boss_run_id` token).
        server_state
            .live_worker_states
            .register_spawn(slot_id, run_id, model, 0, binding);
        tracing::info!(
            run_id,
            slot_id,
            host = %host,
            "live_status: assigned virtual slot to remote worker (no local pane); activity tracks the forwarded hook stream",
        );
        server_state.broadcast_live_worker_states().await;
    }
    Some(slot_id)
}

/// Terminal execution statuses — a hook for one of these is a stray
/// late/duplicate delivery and must not allocate a fresh slot. Mirrors
/// the set used by [`crate::work::WorkDb::list_reattachable_remote_runs`].
fn remote_execution_is_settled(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "abandoned" | "cancelled" | "orphaned"
    )
}

/// The work item's explicit model override, if it carries one (only
/// tasks/chores do). Used to label a remote worker's live state.
fn remote_worker_model_override(item: &boss_protocol::WorkItem) -> Option<String> {
    use boss_protocol::WorkItem;
    match item {
        WorkItem::Task(t) | WorkItem::Chore(t) => t.model_override.clone(),
        WorkItem::Product(_) | WorkItem::Project(_) => None,
    }
}

/// On every `PreToolUse` event whose tool is `Bash` and whose command
/// matches `gh pr|issue {create,edit,comment,review}` (or `cube pr ensure`),
/// evaluate the command against the product's editorial rules and write the
/// decision to `editorial_actions`. Emits a `work_editorial_action` topic
/// event so subscribers (bossctl, kanban) can observe decisions live.
///
/// Fails open on every error: a DB failure, a missing execution row, or an
/// unresolvable product are all logged and dropped. The editorial controls are
/// advisory-in-a-partition — never a hard block on the event loop.
pub(super) async fn dispatch_editorial_on_pretooluse(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
) {
    use crate::protocol::WorkerEvent;
    use boss_editorial::CompiledRules;
    use std::path::Path;

    let WorkerEvent::PreToolUse {
        tool_name,
        tool_input,
        ..
    } = &incoming.event
    else {
        return;
    };
    if tool_name != "Bash" {
        return;
    }
    let command = match tool_input.get("command").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return,
    };

    // Fast path: only evaluate commands that match the editorial hook's scope.
    if !crate::gh_invocation::is_editorial_candidate(command) {
        return;
    }

    let Some(execution_id) = incoming.run_id.as_deref() else {
        return;
    };

    // Load the product_id and editorial_rules in one synchronous query.
    let (product_id, editorial_rules, workspace_path_opt) =
        match server_state.work_db.get_editorial_context(execution_id) {
            Ok(ctx) => ctx,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    ?err,
                    "editorial_pretooluse: could not load editorial context; skipping",
                );
                return;
            }
        };

    if product_id.is_empty() {
        tracing::debug!(
            execution_id,
            "editorial_pretooluse: execution has no product; skipping",
        );
        return;
    }

    // Compile the user-supplied rules (baked-ins always apply inside evaluate_gh_pretooluse).
    let compiled = match CompiledRules::compile(editorial_rules) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(
                execution_id,
                ?err,
                "editorial_pretooluse: could not compile editorial rules; skipping",
            );
            return;
        }
    };

    // Use the workspace path as cwd for --body-file resolution; fall back to
    // an empty path (evaluate_gh_pretooluse fails-open when the file is unreadable).
    let cwd_path: std::path::PathBuf = workspace_path_opt
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new(".").to_path_buf());

    let outcome = crate::editorial_hook::evaluate_gh_pretooluse(
        command,
        &cwd_path,
        &compiled,
        None, // PR template support is a follow-up (chore #9)
        execution_id,
        &server_state.editorial_deny_tracker,
    );

    let action_str = outcome.action.as_str();
    let reason_str: Option<String> = if outcome.findings.is_empty() {
        None
    } else {
        Some(
            outcome
                .findings
                .iter()
                .map(|f| f.description.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        )
    };

    // Best-effort PR URL from the staged cache.
    let pr_url = server_state.staged_pr_urls.get(execution_id);

    // Write to DB.
    let insert_result = server_state.work_db.insert_editorial_action(
        &product_id,
        execution_id,
        pr_url.as_deref(),
        command,
        action_str,
        reason_str.as_deref(),
    );
    let row_id = match insert_result {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(
                execution_id,
                %product_id,
                ?err,
                "editorial_pretooluse: DB insert failed",
            );
            return;
        }
    };

    tracing::info!(
        execution_id,
        %product_id,
        action = action_str,
        row_id,
        "editorial_pretooluse: recorded action",
    );

    // Build the EditorialAction for the topic event.
    use crate::work::now_string;
    let editorial_action = boss_protocol::EditorialAction::builder()
        .id(row_id.to_string())
        .product_id(&product_id)
        .execution_id(execution_id)
        .maybe_pr_url(pr_url)
        .tool_command(command)
        .action(action_str)
        .reason(reason_str.unwrap_or_default())
        .created_at(now_string())
        .build();

    // Emit topic event so subscribers can observe decisions live.
    let revision = server_state
        .work_revision
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        + 1;
    let topic = editorial_actions_topic(&product_id);
    let event = FrontendEvent::TopicEvent {
        topic: topic.clone(),
        revision,
        origin_session_id: String::new(),
        origin_request_id: None,
        event: TopicEventPayload::WorkEditorialAction {
            action: editorial_action,
        },
    };
    server_state
        .topic_broker
        .publish(
            &topic,
            FrontendEventEnvelope::push_with_revision(revision, event),
        )
        .await;
}

/// On `Stop` hook events, pop a pending probe for the run (if any)
/// and `SendToPane` the text to the worker's slot. The injection
/// arrives at the pane just as the worker becomes idle, so claude
/// treats it as the next user prompt. After a successful dispatch,
/// records an in-flight entry (with the transcript path and current
/// byte offset) so `dispatch_probe_reply_on_stop` can emit the
/// matching `FrontendEvent::ProbeReplied` when the next Stop lands.
pub(super) async fn dispatch_probe_on_stop(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
) {
    use crate::protocol::{EngineToAppRequest, SendToPaneInput, WorkerEvent};
    let WorkerEvent::Stop { .. } = incoming.event else {
        return;
    };
    let Some(run_id) = incoming.run_id.as_deref() else {
        return;
    };
    let Some(probe) = server_state.pop_pending_probe(run_id) else {
        return;
    };
    let Some(slot_id) = server_state.worker_registry.slot_for_run(run_id) else {
        tracing::warn!(
            run_id,
            "probe ready but no slot mapping; dropping probe text",
        );
        return;
    };
    // Capture the transcript path + current byte length *before* the
    // dispatch round-trip so we don't accidentally include any
    // assistant content the worker happened to flush while we were
    // still in this code path.
    let (transcript_path, offset_bytes) = transcript_offset_for_run(server_state, run_id).await;
    let request = EngineToAppRequest::SendToPane(SendToPaneInput {
        slot_id,
        text: probe.text.clone(),
    });
    match server_state
        .send_to_app(request, Duration::from_secs(5))
        .await
    {
        Ok(_) => {
            tracing::info!(
                run_id,
                slot_id,
                probe_id = %probe.probe_id,
                "probe injected into pane",
            );
            server_state.note_probe_dispatched(
                run_id.to_owned(),
                probe.probe_id,
                transcript_path,
                offset_bytes,
            );
        }
        Err(err) => {
            tracing::warn!(
                ?err,
                run_id,
                slot_id,
                probe_id = %probe.probe_id,
                "probe injection failed; pushing text back onto queue",
            );
            // Push back on the front so the next Stop retries with
            // the same probe id — callers waiting on the matching
            // `ProbeReplied` event must not see their id silently
            // reissued.
            server_state.requeue_probe_front(run_id.to_owned(), probe);
        }
    }
}

/// On the `PostToolUse` boundary, check whether the front probe in the
/// per-run queue is urgent. If so, pop it and dispatch it immediately
/// via `SendToPane`, prefixing the text with `[coordinator-nudge]` so
/// the worker and human readers can identify coordinator-injected
/// urgent text. The tool call has already completed at this point, so
/// no in-flight Bash is cancelled. On failure the probe is pushed back
/// to the front so the next `PostToolUse` retries with the same id.
///
/// Non-urgent probes are ignored here; they wait for `dispatch_probe_on_stop`.
pub(super) async fn dispatch_urgent_probe_on_post_tool_use(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
) {
    use crate::protocol::{EngineToAppRequest, SendToPaneInput, WorkerEvent};
    let WorkerEvent::PostToolUse { .. } = incoming.event else {
        return;
    };
    let Some(run_id) = incoming.run_id.as_deref() else {
        return;
    };
    // Peek at the front probe and pop it only if it's urgent.
    // The lock must be released before any async call.
    let probe = {
        let mut guard = server_state
            .pending_probes
            .lock()
            .expect("pending_probes mutex poisoned");
        let Some(queue) = guard.get_mut(run_id) else {
            return;
        };
        if !queue.front().map(|p| p.urgent).unwrap_or(false) {
            return;
        }
        let probe = queue.pop_front().unwrap();
        if queue.is_empty() {
            guard.remove(run_id);
        }
        probe
    };
    let Some(slot_id) = server_state.worker_registry.slot_for_run(run_id) else {
        tracing::warn!(
            run_id,
            "urgent probe ready but no slot mapping; dropping probe",
        );
        return;
    };
    let (transcript_path, offset_bytes) = transcript_offset_for_run(server_state, run_id).await;
    let marked_text = format!("[coordinator-nudge] {}", probe.text);
    let request = EngineToAppRequest::SendToPane(SendToPaneInput {
        slot_id,
        text: marked_text,
    });
    match server_state
        .send_to_app(request, Duration::from_secs(5))
        .await
    {
        Ok(_) => {
            tracing::info!(
                run_id,
                slot_id,
                probe_id = %probe.probe_id,
                "urgent probe injected at tool boundary",
            );
            server_state.note_probe_dispatched(
                run_id.to_owned(),
                probe.probe_id,
                transcript_path,
                offset_bytes,
            );
        }
        Err(err) => {
            tracing::warn!(
                ?err,
                run_id,
                slot_id,
                probe_id = %probe.probe_id,
                "urgent probe injection failed; pushing back onto queue",
            );
            server_state.requeue_probe_front(run_id.to_owned(), probe);
        }
    }
}

/// Immediately dispatch a queued probe to `run_id`'s worker pane if
/// the worker is currently idle (i.e. between turns, waiting for
/// input). Called from the `ProbeRun` frontend handler so that
/// `bossctl probe` delivers the text without waiting for the next Stop
/// boundary — a Stop never arrives for a worker that is already idle,
/// so the on-Stop path alone would silently stall these probes.
///
/// If the worker is actively running (Working/WaitingForInput/Spawning)
/// this function is a no-op: the probe stays in `pending_probes` and
/// `dispatch_probe_on_stop` picks it up at the next Stop boundary.
///
/// Uses the same `SendToPane` path as `dispatch_probe_on_stop` and
/// records an in-flight entry so `dispatch_probe_reply_on_stop` can
/// emit `ProbeReplied` when the worker responds.
pub(super) async fn dispatch_probe_if_idle(server_state: &Arc<ServerState>, run_id: &str) {
    use crate::protocol::{EngineToAppRequest, SendToPaneInput};
    use boss_protocol::WorkerActivity;

    let Some(slot_id) = server_state.worker_registry.slot_for_run(run_id) else {
        // Worker not yet mapped to a slot (spawning) — probe stays queued.
        tracing::debug!(
            run_id,
            "probe-if-idle: no slot mapping; probe waits for Stop"
        );
        return;
    };
    let is_idle = server_state
        .live_worker_states
        .get(slot_id)
        .map(|s| s.activity == WorkerActivity::Idle)
        .unwrap_or(false);
    if !is_idle {
        tracing::debug!(
            run_id,
            slot_id,
            "probe-if-idle: worker not idle; probe will fire at next Stop",
        );
        return;
    }

    let Some(probe) = server_state.pop_pending_probe(run_id) else {
        return;
    };
    let (transcript_path, offset_bytes) = transcript_offset_for_run(server_state, run_id).await;
    let request = EngineToAppRequest::SendToPane(SendToPaneInput {
        slot_id,
        text: probe.text.clone(),
    });
    match server_state
        .send_to_app(request, Duration::from_secs(5))
        .await
    {
        Ok(_) => {
            tracing::info!(
                run_id,
                slot_id,
                probe_id = %probe.probe_id,
                "probe injected into idle worker pane (immediate dispatch)",
            );
            server_state.note_probe_dispatched(
                run_id.to_owned(),
                probe.probe_id,
                transcript_path,
                offset_bytes,
            );
        }
        Err(err) => {
            tracing::warn!(
                ?err,
                run_id,
                slot_id,
                probe_id = %probe.probe_id,
                "probe immediate-dispatch failed; pushing back onto queue",
            );
            server_state.requeue_probe_front(run_id.to_owned(), probe);
        }
    }
}

/// Look up the transcript path the run is currently writing to (via
/// `WorkRun`), and stat its current byte size so we can use that as
/// the lower bound for the next reply-extraction read. Returns
/// `(None, 0)` when the run has no transcript path recorded yet —
/// the in-flight bookkeeping still tracks the dispatched probe, but
/// `dispatch_probe_reply_on_stop` will skip emission with a warning
/// rather than fabricate empty reply text.
///
/// The `run_id` argument is the execution id (`exec_*`) carried on
/// the hook event — the same value
/// `LiveStatusManager`/`dispatch_live_worker_state` plumb everywhere
/// in this file. PR #384 flagged this code path as broken (its
/// "Out of scope" section called out that `work_db.get_run(run_id)`
/// was joining the wrong namespace). Fixed here alongside the
/// `TranscriptPathResolver` impl.
async fn transcript_offset_for_run(
    server_state: &Arc<ServerState>,
    run_id: &str,
) -> (Option<String>, u64) {
    let path = match server_state.work_db.transcript_path_for_execution(run_id) {
        Ok(path) => path,
        Err(err) => {
            tracing::debug!(
                run_id,
                ?err,
                "transcript path lookup failed for probe dispatch",
            );
            None
        }
    };
    let Some(path_str) = path else {
        return (None, 0);
    };
    let offset = match tokio::fs::metadata(&path_str).await {
        Ok(meta) => meta.len(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0,
        Err(err) => {
            tracing::warn!(
                run_id,
                path = %path_str,
                ?err,
                "failed to stat transcript at probe dispatch; treating offset as 0",
            );
            0
        }
    };
    (Some(path_str), offset)
}

/// On the `Stop` boundary that follows a probe dispatch, take the
/// in-flight entry for `run_id`, read transcript bytes written since
/// dispatch, and emit `FrontendEvent::ProbeReplied` on the per-run
/// probe topic. Idempotent: a duplicate Stop with no in-flight
/// probe is a no-op, so observers never see the same `probe_id`
/// reported twice.
pub(super) async fn dispatch_probe_reply_on_stop(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
) {
    use crate::protocol::WorkerEvent;
    let WorkerEvent::Stop { .. } = incoming.event else {
        return;
    };
    let Some(run_id) = incoming.run_id.as_deref() else {
        return;
    };
    let Some(in_flight) = server_state.take_in_flight_probe(run_id) else {
        return;
    };
    let Some(transcript_path) = in_flight.transcript_path.as_deref() else {
        tracing::warn!(
            run_id,
            probe_id = %in_flight.probe_id,
            "probe reply skipped: no transcript path was recorded at dispatch",
        );
        return;
    };
    let text = match read_assistant_reply(transcript_path, in_flight.offset_bytes).await {
        Ok(Some(text)) => text,
        Ok(None) => {
            tracing::warn!(
                run_id,
                probe_id = %in_flight.probe_id,
                transcript_path,
                "probe reply skipped: transcript had no assistant turn after dispatch offset",
            );
            return;
        }
        Err(err) => {
            tracing::warn!(
                run_id,
                probe_id = %in_flight.probe_id,
                transcript_path,
                ?err,
                "probe reply skipped: transcript read failed",
            );
            return;
        }
    };
    let envelope = FrontendEventEnvelope::push(FrontendEvent::ProbeReplied {
        run_id: run_id.to_owned(),
        probe_id: in_flight.probe_id.clone(),
        text,
    });
    server_state
        .topic_broker
        .publish(&probe_topic(run_id), envelope)
        .await;
    tracing::info!(
        run_id,
        probe_id = %in_flight.probe_id,
        "probe reply emitted",
    );
}

/// Read transcript bytes from `offset_bytes` to the end of the file
/// at `transcript_path`, parse each new JSONL line, and return the
/// last assistant-turn text found. Returns `Ok(None)` when no
/// assistant turn appears in the new region (e.g. the worker
/// errored out before producing one).
async fn read_assistant_reply(
    transcript_path: &str,
    offset_bytes: u64,
) -> std::io::Result<Option<String>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
    let mut file = tokio::fs::File::open(transcript_path).await?;
    let metadata = file.metadata().await?;
    if metadata.len() <= offset_bytes {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(offset_bytes)).await?;
    let mut buf = Vec::with_capacity((metadata.len() - offset_bytes) as usize);
    file.read_to_end(&mut buf).await?;
    let chunk = match String::from_utf8(buf) {
        Ok(chunk) => chunk,
        Err(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "transcript bytes are not valid utf-8",
            ));
        }
    };
    Ok(extract_last_assistant_text(&chunk))
}

/// Walk JSONL `chunk` and return the most recent assistant turn's
/// text content, concatenating all `text` blocks inside its message.
/// Tolerates the two shapes claude transcripts use today —
/// `message.content[*].text` (current) and `message.text` (older
/// snapshots) — and skips lines that aren't valid JSON rather than
/// rejecting the whole chunk.
pub(super) fn extract_last_assistant_text(chunk: &str) -> Option<String> {
    let mut latest: Option<String> = None;
    for line in chunk.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if value.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        let mut buf = String::new();
        if let Some(content) = message.get("content").and_then(|c| c.as_array()) {
            for block in content {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    buf.push_str(text);
                }
            }
        }
        if buf.is_empty() {
            if let Some(text) = message.get("text").and_then(|t| t.as_str()) {
                buf.push_str(text);
            }
        }
        if !buf.is_empty() {
            latest = Some(buf);
        }
    }
    latest
}

/// On `Stop` hook events, ask the completion handler whether the
/// worker has produced a PR for its workspace branch. If so, the
/// linked task/chore moves to `in_review`, the execution finalises,
/// and the cube workspace is released. If not, an `awaiting_input`
/// signal is published for the execution topic so the pane indicator
/// can reflect that the worker is idle without losing the active
/// kanban state.
///
/// Runs **before** `dispatch_probe_on_stop` in the event loop so that
/// probes the completion handler queues (e.g. `PROBE_NO_PR`) are
/// visible when probe dispatch fires on the same Stop boundary — if
/// completion ran after, those probes would stall until the next Stop
/// (which never arrives for a worker that is already idle).
pub(super) async fn dispatch_completion_on_stop(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
) {
    use crate::protocol::WorkerEvent;
    let WorkerEvent::Stop { .. } = incoming.event else {
        return;
    };
    let Some(run_id) = incoming.run_id.as_deref() else {
        return;
    };
    let outcome = server_state.completion_handler.on_stop(run_id).await;
    // Info-level so non-success outcomes (DetectorFailed, AwaitingInput,
    // StalePr, EmptyDiffPr) appear in the engine log without enabling
    // debug. The 2026-05-13 three-concurrent-workers regression had
    // zero log evidence because this was at debug — operators saw
    // `activity=idle` workers but no record of what `on_stop` returned.
    tracing::info!(run_id, ?outcome, "completion handler stop result");
}
