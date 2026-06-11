//! Handler helper functions shared across `FrontendRequest` handler submodules.
//!
//! Split out of `app.rs`; all small utility functions used by multiple
//! handler modules live here. Pure structural move — no behavioural change.

use super::*;

/// Build the per-product effort-audit report. Handles the product
/// lookup, window filter, and chore-corpus / event-log fan-in so
/// the RPC handler stays a thin error-translation layer.
pub(super) fn build_effort_audit_report(
    work_db: &WorkDb,
    product_id: &str,
    window_days: Option<u32>,
) -> Result<boss_protocol::EffortAuditReport> {
    let product = work_db
        .get_product(product_id)?
        .ok_or_else(|| anyhow::anyhow!("unknown product: {product_id}"))?;
    let since_epoch_secs = window_days.and_then(|days| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs() as i64;
        let span = (days as i64).saturating_mul(86_400);
        Some(now - span)
    });
    let events = work_db.list_effort_escalations_for_product(&product.id, since_epoch_secs)?;
    let chores = work_db.list_chores_for_audit(&product.id)?;
    let generated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default();
    Ok(audit_effort::build_report(
        &product.id,
        &product.slug,
        window_days,
        &chores,
        &events,
        generated_at,
    ))
}

/// Metadata key used to persist the live-status disabled-slot list.
/// Stored as a comma-separated list of u8 slot ids — the set is at
/// most 8 entries, so we don't bother with JSON.
const META_LIVE_STATUS_DISABLED_SLOTS: &str = "live_status_disabled_slots";

/// Metadata key for the global dispatch-pause flag. `"1"` = paused, `"0"` or
/// absent = running. Persisted at every toggle so the pause survives an engine
/// restart.
pub(super) const METADATA_KEY_DISPATCH_PAUSED: &str = "dispatch_paused";
/// Metadata key storing the epoch-seconds timestamp at which dispatch was last
/// paused. Zero (or absent) means not paused.
pub(super) const METADATA_KEY_DISPATCH_PAUSED_SINCE: &str = "dispatch_paused_since_epoch_s";

/// Persist the disabled-slot snapshot to the metadata KV. Called
/// from the toggle handler. Errors bubble up so the caller can log
/// them — persistence failure is non-fatal (the in-memory set still
/// honours the toggle until restart).
pub(super) fn persist_live_status_disabled_slots(work_db: &WorkDb, slot_ids: &[u8]) -> Result<()> {
    let joined = slot_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(",");
    work_db.set_metadata(META_LIVE_STATUS_DISABLED_SLOTS, &joined)?;
    Ok(())
}

/// Build the user-visible engine health snapshot returned by
/// [`FrontendRequest::GetEngineHealth`]. The macOS app polls this on
/// session start so the banner / settings warning lands before the
/// user notices that summarization isn't producing output.
///
/// Currently checks one thing — `ANTHROPIC_API_KEY` presence — but
/// the shape is the list-of-issues form the chore brief asked for so
/// subsequent missing-config surfaces (engine socket, cube binary,
/// etc.) can be added without bumping the wire format.
pub(super) fn build_engine_health_report(server_state: &Arc<ServerState>) -> boss_protocol::EngineHealthReport {
    use boss_protocol::{EngineHealthIssue, EngineHealthReport};

    let anthropic_api_key_present = server_state.anthropic_api_key.is_some();
    let dispatch_paused = server_state.execution_coordinator.is_dispatch_paused();
    let mut issues: Vec<EngineHealthIssue> = Vec::new();

    if !anthropic_api_key_present {
        issues.push(EngineHealthIssue {
            kind: "missing_anthropic_api_key".to_owned(),
            severity: "warning".to_owned(),
            title: "ANTHROPIC_API_KEY is not set".to_owned(),
            body: "Live worker summaries and pane summarization are \
                   disabled until ANTHROPIC_API_KEY is exported in the \
                   environment Boss launches its engine from. Set the \
                   variable in your shell startup file, then quit and \
                   relaunch Boss to pick it up."
                .to_owned(),
        });
    }

    if dispatch_paused {
        issues.push(EngineHealthIssue {
            kind: "dispatch_paused".to_owned(),
            severity: "warning".to_owned(),
            title: "Dispatch is globally paused".to_owned(),
            body: "The engine is not dispatching new executions from any source. \
                   Currently-running workers continue to completion. Run \
                   `bossctl dispatch resume` to restore normal dispatch."
                .to_owned(),
        });
    }

    // macOS `syspolicyd` wedge: when the code-signing daemon is pinned at
    // ~100% CPU it stalls every build machine-wide. Surface it as an
    // error with the recovery steps so the operator doesn't waste time
    // expunging Bazel caches (which won't help). See
    // [`crate::syspolicyd_monitor`].
    let syspolicyd = server_state.syspolicyd_health.snapshot();
    if syspolicyd.wedged {
        let pid_str = syspolicyd
            .pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "<pid>".to_owned());
        issues.push(EngineHealthIssue {
            kind: "syspolicyd_wedged".to_owned(),
            severity: "error".to_owned(),
            title: "macOS syspolicyd is wedged — all builds are stalled".to_owned(),
            body: format!(
                "syspolicyd (pid {pid_str}) is pinned at ~{cpu:.0}% CPU and has stopped \
                 servicing code-signing assessments. While it's stuck, every dlopen of a \
                 signature-checked library blocks, so all Bazel servers hang at JVM startup \
                 (\"Starting local Bazel server… still trying to connect\", then exit 37 \
                 \"Server crashed during startup\") and every build on this machine stalls.\n\
                 \n\
                 This is a macOS fault, not a Boss bug — Boss's workload (rapidly launching \
                 freshly-built, ad-hoc-signed binaries) reliably triggers it. Killing or \
                 expunging Bazel will NOT help, because the bottleneck is the system daemon.\n\
                 \n\
                 Remedy: run `sudo kill -9 {pid_str}` — launchd will relaunch a fresh \
                 syspolicyd (SIP blocks `launchctl kickstart` of it). A reboot is the \
                 fallback. Note: Bazel servers already blocked in the kernel do NOT recover \
                 when syspolicyd restarts — kill those hung servers individually.",
                cpu = syspolicyd.cpu_pct,
            ),
        });
    }

    EngineHealthReport {
        anthropic_api_key_present,
        dispatch_paused,
        issues,
    }
}

/// Build the per-slot diagnostic snapshot the `live-status debug`
/// verb returns. Reads the manager's debug store, joins with the
/// per-slot live state (for transcript_path lookup via WorkDb), and
/// stamps engine-level facts (build SHA, API key presence). No
/// blocking IO is acceptable here — this verb is called interactively
/// and must return promptly even when the engine is busy.
pub(super) fn build_live_status_debug_report(
    server_state: &Arc<ServerState>,
    work_db: &WorkDb,
) -> boss_protocol::LiveStatusDebugReport {
    use boss_protocol::{LiveStatusDebugReport, LiveStatusSlotDebug};
    let manager = &server_state.live_status_manager;
    let live_states = server_state.live_worker_states.snapshot();
    let store = manager.debug_store();
    let store_snapshots = store.snapshot_all();
    let active_slots = manager.active_slot_ids();
    let disabled_set: std::collections::HashSet<u8> = manager.disabled_snapshot().into_iter().collect();

    // Union of every slot id we have *any* signal for — live state,
    // diagnostic snapshot, or active task. Sorted ascending so the
    // table renderer can walk in order.
    let mut slot_ids: std::collections::BTreeSet<u8> = std::collections::BTreeSet::new();
    slot_ids.extend(live_states.iter().map(|s| s.slot_id));
    slot_ids.extend(store_snapshots.keys().copied());
    slot_ids.extend(active_slots.iter().copied());
    slot_ids.extend(disabled_set.iter().copied());

    let mut slots: Vec<LiveStatusSlotDebug> = Vec::with_capacity(slot_ids.len());
    for slot_id in slot_ids {
        let snap = store_snapshots.get(&slot_id).cloned().unwrap_or_default();
        let live = live_states.iter().find(|s| s.slot_id == slot_id);
        // Prefer the live-state run id (always present if there's a
        // live entry) over the registry walk: a slot whose worker has
        // just been released will have a snapshot frozen with the
        // prior run's transcript path, which is more honest than a
        // None.
        //
        // `live.run_id` here is actually the execution id — see the
        // resolver impl at the top of this file. The pre-fix fallback
        // called `work_db.get_run(run_id)` (joining on `work_runs.id`),
        // which produced `Err(unknown run)` every time and silently
        // collapsed to `None`. That left `transcript_path: null` on the
        // slot snapshot even when the underlying `work_runs` row had
        // the column populated.
        let transcript_path = snap.transcript_path.clone().or_else(|| {
            let execution_id = live.map(|s| s.run_id.as_str())?;
            work_db.transcript_path_for_execution(execution_id).ok().flatten()
        });
        slots.push(LiveStatusSlotDebug {
            slot_id,
            task_running: active_slots.contains(&slot_id),
            disabled: disabled_set.contains(&slot_id),
            last_trigger_kind: snap.last_trigger_kind.clone(),
            last_trigger_at: snap.last_trigger_at_epoch_s.map(format_epoch_iso8601),
            last_real_trigger_kind: snap.last_real_trigger_kind.clone(),
            last_real_trigger_at: snap.last_real_trigger_at_epoch_s.map(format_epoch_iso8601),
            last_synthetic_trigger_at: snap.last_synthetic_trigger_at_epoch_s.map(format_epoch_iso8601),
            last_outcome_tag: snap.last_outcome_tag.clone(),
            last_outcome_detail: snap.last_outcome_detail.clone(),
            last_outcome_at: snap.last_outcome_at_epoch_s.map(format_epoch_iso8601),
            last_success_at: snap.last_success_at_epoch_s.map(format_epoch_iso8601),
            last_success_text: snap.last_success_text.clone(),
            transcript_path,
            last_redacted_bytes: snap.last_redacted_bytes.map(|n| n as u64),
        });
    }

    let stats = server_state.dispatcher_stats.snapshot();
    let dispatcher_stats = boss_protocol::DispatcherStatsReport {
        hook_events_total: stats.hook_events_total,
        hook_events_dropped_missing_run_id: stats.hook_events_dropped_missing_run_id,
        hook_events_with_transcript_path_in_payload: stats.hook_events_with_transcript_path_in_payload,
        hook_events_without_transcript_path_in_payload: stats.hook_events_without_transcript_path_in_payload,
        transcript_path_persist_updated: stats.transcript_path_persist_updated,
        transcript_path_persist_noop: stats.transcript_path_persist_noop,
        transcript_path_persist_row_missing: stats.transcript_path_persist_row_missing,
        transcript_path_persist_err: stats.transcript_path_persist_err,
        transcript_path_persist_from_cache: stats.transcript_path_persist_from_cache,
        last_hook_run_id: stats.last_hook.as_ref().map(|h| h.run_id.clone()),
        last_hook_kind: stats.last_hook.as_ref().map(|h| h.kind.clone()),
        last_hook_at: stats.last_hook.as_ref().map(|h| format_epoch_iso8601(h.epoch_s)),
    };

    LiveStatusDebugReport {
        engine_build_sha: crate::build_info::git_sha().to_owned(),
        engine_build_time: crate::build_info::build_time().to_owned(),
        engine_binary_fingerprint: crate::build_info::binary_fingerprint().to_owned(),
        engine_process_started_at: crate::build_info::process_started_at().to_owned(),
        dispatcher_stats,
        anthropic_api_key_present: server_state.anthropic_api_key.is_some(),
        tracked_slot_count: active_slots.len(),
        disabled_slot_count: disabled_set.len(),
        slots,
    }
}

pub(super) fn format_epoch_iso8601(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let seconds_in_day = epoch_secs.rem_euclid(86_400);
    let hour = seconds_in_day / 3_600;
    let minute = (seconds_in_day % 3_600) / 60;
    let second = seconds_in_day % 60;
    let (year, month, day) = ymd_from_days_since_1970(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn ymd_from_days_since_1970(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Read the persisted disabled-slot list from the metadata KV.
/// Returns an empty vec on first boot or if the row is missing /
/// malformed (a stray comma is treated as "no entries" rather than
/// failing the engine startup).
pub(super) fn load_live_status_disabled_slots(work_db: &WorkDb) -> Vec<u8> {
    let Ok(Some(raw)) = work_db.get_metadata(META_LIVE_STATUS_DISABLED_SLOTS) else {
        return Vec::new();
    };
    raw.split(',').filter_map(|s| s.trim().parse::<u8>().ok()).collect()
}

/// Read the persisted dispatch-pause state from the metadata KV. Returns
/// `(paused, paused_since_epoch_s)`. On first boot or if absent/malformed
/// defaults to `(false, 0)`.
pub(super) fn load_dispatch_paused_state(work_db: &WorkDb) -> (bool, u64) {
    let paused = work_db
        .get_metadata(METADATA_KEY_DISPATCH_PAUSED)
        .ok()
        .flatten()
        .map(|v| v == "1")
        .unwrap_or(false);
    let since_epoch_s = work_db
        .get_metadata(METADATA_KEY_DISPATCH_PAUSED_SINCE)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    (paused, since_epoch_s)
}

/// Downcast `err` to `DuplicateTaskError` and return a structured
/// `WorkItemDuplicateBlocked` event; fall back to `WorkError` for any
/// other error kind. Keeps the `CreateTask` / `CreateChore` error arms
/// DRY.
pub(super) fn duplicate_or_work_error(err: anyhow::Error) -> FrontendEvent {
    if let Some(dup) = err.downcast_ref::<DuplicateTaskError>() {
        FrontendEvent::WorkItemDuplicateBlocked {
            existing_id: dup.existing_id.clone(),
            existing_short_id: dup.existing_short_id,
            name: dup.name.clone(),
            age_secs: dup.age_secs,
        }
    } else {
        FrontendEvent::WorkError {
            message: err.to_string(),
        }
    }
}

pub(super) fn send_response(sink: &SessionSink, request_id: &str, payload: FrontendEvent) {
    sink.enqueue(FrontendEventEnvelope::response(request_id.to_owned(), payload));
}

pub(super) fn send_response_with_revision(sink: &SessionSink, request_id: &str, revision: u64, payload: FrontendEvent) {
    sink.enqueue(FrontendEventEnvelope::response_with_revision(
        request_id.to_owned(),
        revision,
        payload,
    ));
}

pub(super) fn send_push(sink: &SessionSink, payload: FrontendEvent) {
    sink.enqueue(FrontendEventEnvelope::push(payload));
}

pub(super) async fn publish_work_invalidation(
    server_state: &ServerState,
    origin_session_id: &str,
    origin_request_id: &str,
    topics: Vec<String>,
    reason: &str,
    product_id: Option<String>,
    item_ids: Vec<String>,
) -> u64 {
    if let Some(product_id) = product_id.as_deref() {
        match server_state.work_db.reconcile_product_executions(product_id) {
            Ok(result) => {
                if !result.created.is_empty() || !result.updated.is_empty() {
                    tracing::info!(
                        product_id,
                        created = result.created.len(),
                        updated = result.updated.len(),
                        "reconciled product executions"
                    );
                }
            }
            Err(err) => {
                tracing::error!(
                    ?err,
                    product_id,
                    "failed to reconcile product executions after work invalidation"
                );
            }
        }

        let coordinator = server_state.execution_coordinator.clone();
        coordinator.kick();
    }

    let revision = server_state.bump_work_revision();
    let event = FrontendEvent::TopicEvent {
        topic: String::new(),
        revision,
        origin_session_id: origin_session_id.to_owned(),
        origin_request_id: Some(origin_request_id.to_owned()),
        event: TopicEventPayload::WorkInvalidated {
            reason: reason.to_owned(),
            product_id,
            item_ids,
        },
    };

    let mut unique_topics = HashSet::new();
    for topic in topics {
        if !unique_topics.insert(topic.clone()) {
            continue;
        }
        let mut event = event.clone();
        if let FrontendEvent::TopicEvent { topic: event_topic, .. } = &mut event {
            *event_topic = topic.clone();
        }
        server_state
            .topic_broker
            .publish(&topic, FrontendEventEnvelope::push_with_revision(revision, event))
            .await;
    }

    revision
}

/// Publish a comment-artifact invalidation on the `comments.artifact.*`
/// topic so other subscribers refetch. Deliberately lighter than
/// [`publish_work_invalidation`]: comment changes never touch the
/// work-item / execution graph, so this skips the product-execution
/// reconcile + coordinator kick. Reuses the `WorkInvalidated` payload
/// (invalidation-not-patch) carrying the artifact id as the sole item id.
pub(super) async fn publish_comment_invalidation(
    server_state: &ServerState,
    origin_session_id: &str,
    origin_request_id: &str,
    artifact_kind: &str,
    artifact_id: &str,
    reason: &str,
) -> u64 {
    let revision = server_state.bump_work_revision();
    let topic = comment_topic(artifact_kind, artifact_id);
    let event = FrontendEvent::TopicEvent {
        topic: topic.clone(),
        revision,
        origin_session_id: origin_session_id.to_owned(),
        origin_request_id: Some(origin_request_id.to_owned()),
        event: TopicEventPayload::WorkInvalidated {
            reason: reason.to_owned(),
            product_id: None,
            item_ids: vec![artifact_id.to_owned()],
        },
    };
    server_state
        .topic_broker
        .publish(&topic, FrontendEventEnvelope::push_with_revision(revision, event))
        .await;
    revision
}

/// Bulk counterpart of [`publish_work_invalidation`]. Emits one
/// `WorkInvalidated` topic event per distinct `product_id` carrying
/// only that product's item ids, all at the same fresh revision —
/// kanban consumers reload their product once. Returns the shared
/// revision so the caller can stamp it on the unicast response.
pub(super) async fn publish_batch_work_invalidation(
    server_state: &ServerState,
    origin_session_id: &str,
    origin_request_id: &str,
    reason: &str,
    items: &[WorkItem],
) -> u64 {
    let mut by_product: HashMap<String, Vec<String>> = HashMap::new();
    for item in items {
        by_product
            .entry(work_item_product_id(item))
            .or_default()
            .push(work_item_id(item));
    }

    for product_id in by_product.keys() {
        match server_state.work_db.reconcile_product_executions(product_id) {
            Ok(result) => {
                if !result.created.is_empty() || !result.updated.is_empty() {
                    tracing::info!(
                        product_id,
                        created = result.created.len(),
                        updated = result.updated.len(),
                        "reconciled product executions",
                    );
                }
            }
            Err(err) => {
                tracing::error!(
                    ?err,
                    product_id,
                    "failed to reconcile product executions after batch create",
                );
            }
        }
    }

    if !by_product.is_empty() {
        server_state.execution_coordinator.clone().kick();
    }

    let revision = server_state.bump_work_revision();
    for (product_id, item_ids) in by_product {
        let topic = work_product_topic(&product_id);
        let event = FrontendEvent::TopicEvent {
            topic: topic.clone(),
            revision,
            origin_session_id: origin_session_id.to_owned(),
            origin_request_id: Some(origin_request_id.to_owned()),
            event: TopicEventPayload::WorkInvalidated {
                reason: reason.to_owned(),
                product_id: Some(product_id),
                item_ids,
            },
        };
        server_state
            .topic_broker
            .publish(&topic, FrontendEventEnvelope::push_with_revision(revision, event))
            .await;
    }

    revision
}

/// Common dispatch for the two batch-create requests. Wraps the
/// engine-level result, builds the per-item `WorkItem` list, fans
/// out a `WorkInvalidated` topic event per distinct product, and
/// replies to the caller with a single `WorkItemsCreated` event
/// (or a `WorkError` on failure — the engine work_db rolled the
/// transaction back atomically).
pub(super) async fn handle_create_many(
    db_result: anyhow::Result<Vec<Task>>,
    reason: &str,
    wrap: fn(Task) -> WorkItem,
    server_state: &Arc<ServerState>,
    session_id: &str,
    request_id: &str,
    sink: &SessionSink,
) {
    match db_result {
        Ok(rows) => {
            let items: Vec<WorkItem> = rows.into_iter().map(wrap).collect();
            let revision = publish_batch_work_invalidation(server_state, session_id, request_id, reason, &items).await;
            send_response_with_revision(sink, request_id, revision, FrontendEvent::WorkItemsCreated { items });
        }
        Err(err) => {
            send_response(sink, request_id, duplicate_or_work_error(err));
        }
    }
}

/// Orchestrate the review-terminal workspace setup for
/// [`FrontendRequest::OpenReviewTerminal`].
///
/// 1. Ensure the repo is registered with cube.
/// 2. Lease a workspace for that repo.
/// 3. Resolve the PR head branch via `gh pr view`.
/// 4. Fetch remote state with `jj git fetch`.
/// 5. Create a new jj commit atop `<branch>@origin` with `jj new`.
/// 6. Return `(workspace_path, lease_id)` to the caller.
///
/// On any failure after a lease is acquired, the lease is released
/// before returning the error so we don't leak idle workspaces.
pub(super) async fn open_review_terminal_async(
    cube_client: &Arc<dyn CubeClient>,
    repo_remote_url: &str,
    pr_url: &str,
    work_item_id: &str,
) -> Result<(String, String)> {
    // Step 1: ensure repo
    let repo = cube_client
        .ensure_repo(repo_remote_url)
        .await
        .with_context(|| format!("cube repo ensure failed for {repo_remote_url}"))?;

    // Step 2: lease workspace
    let task_label = format!("review terminal for {work_item_id}");
    let lease = cube_client
        .lease_workspace(&repo.repo_id, &task_label, None, false, None)
        .await
        .with_context(|| format!("cube workspace lease failed for repo {}", repo.repo_id))?;

    // Helper: release lease and propagate the original error.
    macro_rules! fail_with_release {
        ($err:expr) => {{
            let e = $err;
            let _ = cube_client.release_workspace(&lease.lease_id).await;
            return Err(e);
        }};
    }

    // Step 3: resolve PR head branch
    let head_branch = match get_pr_head_branch(pr_url).await {
        Ok(b) => b,
        Err(e) => fail_with_release!(e),
    };

    // Step 4: jj git fetch
    let fetch_out = TokioCommand::new("jj")
        .args(["git", "fetch"])
        .current_dir(&lease.workspace_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await;
    match fetch_out {
        Err(e) => fail_with_release!(anyhow::anyhow!("failed to spawn jj git fetch: {e}")),
        Ok(o) if !o.status.success() => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            fail_with_release!(anyhow::anyhow!("jj git fetch failed: {}", stderr.trim()))
        }
        Ok(_) => {}
    }

    // Step 5: jj new -r <branch>@origin
    let rev = format!("{head_branch}@origin");
    let new_out = TokioCommand::new("jj")
        .args(["new", "-r", &rev])
        .current_dir(&lease.workspace_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await;
    match new_out {
        Err(e) => fail_with_release!(anyhow::anyhow!("failed to spawn jj new: {e}")),
        Ok(o) if !o.status.success() => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            fail_with_release!(anyhow::anyhow!("jj new -r {rev} failed: {}", stderr.trim()))
        }
        Ok(_) => {}
    }

    Ok((lease.workspace_path.display().to_string(), lease.lease_id))
}

/// Call `gh pr view <pr_url> --json headRefName --jq .headRefName` and
/// return the head branch name. Mirrors the approach in
/// `design_detector::do_scan_pr` but requests only the one field we need.
pub(super) async fn get_pr_head_branch(pr_url: &str) -> Result<String> {
    let output = TokioCommand::new("gh")
        .args(["pr", "view", pr_url, "--json", "headRefName", "--jq", ".headRefName"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("failed to spawn gh pr view for {pr_url}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh pr view {pr_url} failed: {}", stderr.trim());
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if branch.is_empty() {
        anyhow::bail!("gh pr view {pr_url} returned empty headRefName");
    }
    Ok(branch)
}

/// Transport-layer fallback for `created_via` when a caller didn't
/// stamp it themselves. The macOS app self-identifies via
/// `RegisterAppSession`, so any request from the registered app
/// session defaults to `mac_app`; everything else (CLI, bossctl,
/// ad-hoc test client) falls through to `unknown`. CLI / bossctl
/// always set the field explicitly, so `unknown` here only fires for
/// off-the-beaten-path callers — exactly the case we want to flag in
/// the database rather than mislabel.
pub(super) async fn transport_default_created_via(server_state: &Arc<ServerState>, session_id: &str) -> String {
    let app_session_id = server_state
        .app_session
        .lock()
        .await
        .as_ref()
        .map(|h| h.session_id.clone());
    if app_session_id.as_deref() == Some(session_id) {
        boss_protocol::CREATED_VIA_MAC_APP.to_owned()
    } else {
        boss_protocol::CREATED_VIA_UNKNOWN.to_owned()
    }
}

pub(super) fn work_item_id(item: &WorkItem) -> String {
    match item {
        WorkItem::Product(product) => product.id.clone(),
        WorkItem::Project(project) => project.id.clone(),
        WorkItem::Task(task) | WorkItem::Chore(task) => task.id.clone(),
    }
}

/// Validate a kind-specific external tracker config JSON.
/// Returns `Err` with a human-readable message when validation fails.
pub(super) fn validate_external_tracker_config(kind: &str, config: &serde_json::Value) -> Result<(), String> {
    match kind {
        "github" => {
            for field in ["org", "repo"] {
                match config.get(field).and_then(|v| v.as_str()) {
                    None | Some("") => {
                        return Err(format!("missing required field '{field}' for kind=github"));
                    }
                    _ => {}
                }
            }
            match config.get("project_number") {
                None => {
                    return Err("missing required field 'project_number' for kind=github".to_owned());
                }
                Some(v) if !v.is_number() => {
                    return Err("'project_number' must be a number for kind=github".to_owned());
                }
                _ => {}
            }
            Ok(())
        }
        other => Err(format!("unknown tracker kind '{other}'; supported: github")),
    }
}

pub(super) fn work_item_product_id(item: &WorkItem) -> String {
    match item {
        WorkItem::Product(product) => product.id.clone(),
        WorkItem::Project(project) => project.product_id.clone(),
        WorkItem::Task(task) | WorkItem::Chore(task) => task.product_id.clone(),
    }
}

/// Look up the current `tasks.status` for `id`, returning `None` if
/// `id` does not name a task/chore or the work item can't be loaded
/// (already deleted, garbled id). Used by the UpdateWorkItem handler
/// to detect a transition into `active` so it can auto-dispatch.
pub(super) fn task_status_for_id(work_db: &WorkDb, id: &str) -> Option<TaskStatus> {
    match work_db.get_work_item(id) {
        Ok(WorkItem::Task(task)) | Ok(WorkItem::Chore(task)) => Some(task.status),
        Ok(_) => None,
        Err(_) => None,
    }
}

/// True iff the work item has no execution at all, or its latest
/// execution is in a terminal status. Used by the UpdateWorkItem
/// handler's drop-into-Doing dispatch to decide whether to create a
/// fresh execution after a human flips status to `active`. An
/// existing non-terminal execution (`ready` / `running` /
/// `waiting_*`) already owns the dispatch slot, so we leave it alone
/// — the steady-state rescan and the dispatcher's normal flow take
/// care of stale ones.
pub(super) fn work_item_needs_dispatch(work_db: &WorkDb, work_item_id: &str) -> bool {
    match work_db.latest_execution_for_work_item(work_item_id) {
        Ok(Some(existing)) => matches!(
            existing.status.as_str(),
            "completed" | "failed" | "abandoned" | "cancelled" | "orphaned"
        ),
        Ok(None) => true,
        Err(err) => {
            tracing::warn!(
                %work_item_id,
                ?err,
                "work_item_needs_dispatch: failed to read latest execution; skipping auto-dispatch",
            );
            false
        }
    }
}

/// True iff `item` is a task/chore whose status just flipped from
/// something other than `active` to `active`. Re-applying an `active`
/// status on top of `active` (idempotent client retry) does not count.
pub(super) fn task_transitioned_to_active(previous_status: &Option<TaskStatus>, item: &WorkItem) -> bool {
    let task = match item {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        _ => return false,
    };
    if task.status != TaskStatus::Active {
        return false;
    }
    match previous_status {
        Some(prev) => prev != &TaskStatus::Active,
        // We didn't see the row before the update — assume this is the
        // first time the engine has rendered it and treat it as a real
        // transition. Idempotent `request_execution_with_live_check`
        // collapses the duplicate-spawn case safely.
        None => true,
    }
}

/// If `item` is a task or chore that has just landed in a terminal
/// status (`done`, `archived`, `cancelled`), return the id of its
/// most recent execution so the caller can tear down its worker pane
/// and cube workspace. Returns `None` for non-task work items, for
/// non-terminal statuses, and when the work item has no executions.
pub(super) fn terminal_chore_execution(work_db: &WorkDb, item: &WorkItem) -> Option<String> {
    let task = match item {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        _ => return None,
    };
    if !matches!(
        task.status,
        TaskStatus::Done | TaskStatus::Archived | TaskStatus::Cancelled
    ) {
        return None;
    }
    match work_db.latest_execution_for_work_item(&task.id) {
        Ok(Some(execution)) => Some(execution.id),
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                work_item_id = %task.id,
                ?err,
                "terminal_chore_execution: failed to look up latest execution",
            );
            None
        }
    }
}

/// If `item` is a task or chore that has just entered `in_review`
/// status, return the id of its most recent execution so the caller
/// can tear down its worker pane and cube workspace. Returns `None`
/// for non-task work items, for non-`in_review` statuses, and when
/// the work item has no executions.
///
/// Covers the human-drag kanban path. The worker auto-transition path
/// (Stop hook → `finalize_pr_transition`) handles its own teardown
/// inline; this function is the reconciliation safety net.
pub(super) fn in_review_chore_execution(work_db: &WorkDb, item: &WorkItem) -> Option<String> {
    let task = match item {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        _ => return None,
    };
    if task.status != TaskStatus::InReview {
        return None;
    }
    match work_db.latest_execution_for_work_item(&task.id) {
        Ok(Some(execution)) => Some(execution.id),
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                work_item_id = %task.id,
                ?err,
                "in_review_chore_execution: failed to look up latest execution",
            );
            None
        }
    }
}

/// If `item` is a task or chore whose status just changed from `active`
/// to `todo` (i.e., the user dragged an agent-assigned Doing card back
/// to Backlog), return the id of its most recent execution so the caller
/// can cancel the worker and release its resources.
///
/// Returns `None` for non-task work items, when the current status is
/// not `todo`, when the previous status was not `active`, and when the
/// work item has no executions.
pub(super) fn active_to_todo_execution(
    work_db: &WorkDb,
    previous_status: &Option<TaskStatus>,
    item: &WorkItem,
) -> Option<String> {
    let task = match item {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        _ => return None,
    };
    if task.status != TaskStatus::Todo {
        return None;
    }
    if previous_status.as_ref() != Some(&TaskStatus::Active) {
        return None;
    }
    match work_db.latest_execution_for_work_item(&task.id) {
        Ok(Some(execution)) => Some(execution.id),
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                work_item_id = %task.id,
                ?err,
                "active_to_todo_execution: failed to look up latest execution",
            );
            None
        }
    }
}

/// If `item` is a task or chore whose latest execution is still live
/// (non-terminal), return that execution's id so a caller deleting the
/// work item can tear down the worker. A live execution
/// (`ready` / `running` / `waiting_*`) is still holding a worker pool
/// slot and possibly a leased cube workspace; deleting the backing row
/// must stop it, or the agent keeps running with no work item behind
/// it. Terminal executions (`completed` / `failed` / `abandoned` /
/// `cancelled` / `orphaned`) need no teardown.
///
/// Note: this must be read *before* `delete_work_item` tombstones the
/// task row — `latest_execution_for_work_item` queries the
/// `work_executions` table by `work_item_id`, which the soft-delete
/// does not touch, but loading the `WorkItem` itself goes through paths
/// that filter out `deleted_at` rows. The delete handler captures the
/// `WorkItem` before deleting and passes it here.
pub(super) fn live_execution_for_deleted_item(work_db: &WorkDb, item: &WorkItem) -> Option<String> {
    let task = match item {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        _ => return None,
    };
    match work_db.latest_execution_for_work_item(&task.id) {
        Ok(Some(execution))
            if !matches!(
                execution.status.as_str(),
                "completed" | "failed" | "abandoned" | "cancelled" | "orphaned"
            ) =>
        {
            Some(execution.id)
        }
        Ok(_) => None,
        Err(err) => {
            tracing::warn!(
                work_item_id = %task.id,
                ?err,
                "live_execution_for_deleted_item: failed to look up latest execution",
            );
            None
        }
    }
}

/// Return `(name, description)` for a task/chore id, or `None` when
/// the id does not name a task/chore or cannot be read from the DB.
/// Used by the `UpdateWorkItem` handler to snapshot the spec before an
/// edit so the chore-update worker notification can show old vs. new.
pub(super) fn task_name_description_for_id(work_db: &WorkDb, id: &str) -> Option<(String, String)> {
    match work_db.get_work_item(id) {
        Ok(WorkItem::Task(t)) | Ok(WorkItem::Chore(t)) => Some((t.name, t.description)),
        Ok(_) => None,
        Err(_) => None,
    }
}

/// Return the `run_id` of the live worker currently bound to `item`
/// when `item` is an active task/chore with a non-terminal registry
/// entry. Returns `None` for products/projects, for statuses other than
/// `active`, and when no live worker slot carries this item's id.
pub(super) fn active_chore_run_id(server_state: &ServerState, item: &WorkItem) -> Option<String> {
    let task = match item {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        _ => return None,
    };
    if task.status != TaskStatus::Active {
        return None;
    }
    server_state.live_worker_states.run_id_for_work_item(&task.id)
}

/// Build the `[chore-update]` notice text. Returns `None` when neither
/// name nor description actually changed (so the caller can skip the
/// send).
pub(super) fn build_chore_update_message(
    old_name: &str,
    new_name: &str,
    old_description: &str,
    new_description: &str,
) -> Option<String> {
    if old_name == new_name && old_description == new_description {
        return None;
    }
    let mut changes = Vec::new();
    if old_name != new_name {
        changes.push(format!("- name: \"{}\" → \"{}\"", old_name, new_name));
    }
    if old_description != new_description {
        changes.push(format!(
            "- description: \"{}\" → \"{}\"",
            old_description, new_description
        ));
    }
    let body = changes.join("\n");
    Some(format!(
        "[chore-update] The chore you're working on was edited.\nField changes:\n{body}\nPlease re-read the spec and adjust your in-flight work to match. If the change invalidates work you've already done, surface that in your final response.\n"
    ))
}

/// Read the trailing `lines` lines of `transcript_path`. Returns the
/// raw line contents (no trailing newline) plus a flag indicating
/// whether the file held more lines than were returned.
///
/// The transcript file is expected to be JSONL the worker writes
/// incrementally; this helper does not parse it, so the caller can
/// decide how to render. A missing file is reported as an io error
/// instead of returning an empty result so callers can distinguish
/// "no transcript yet" from "transcript is empty".
/// Machine-parseable prefix for the "transcript not yet available"
/// WorkError. Callers can match against this to distinguish a live
/// worker whose first transcript-bearing hook hasn't fired yet
/// (transient, retry) from a run id that's genuinely unknown to the
/// engine (terminal, surface as user error). Keep stable — the
/// coordinator parses it.
pub(super) const TRANSCRIPT_NOT_YET_AVAILABLE_PREFIX: &str = "transcript not yet available for run ";

/// Outcome of [`resolve_transcript_for_tail`].
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TranscriptResolution {
    /// A transcript path was resolved and can be read.
    Found { transcript_path: String },
    /// The id refers to a worker the engine knows is live (or whose
    /// execution row exists) but no hook event has yet carried a
    /// `transcript_path` for it — the dispatcher hasn't populated the
    /// column or the in-memory cache yet. Surfacing this separately
    /// from `Unknown` is the structural fix for the 2026-05-12
    /// incident where `bossctl agents list` knew about a live run but
    /// `bossctl agents transcript` rejected the same id as `unknown
    /// run`, breaking the coordinator's diagnostic path.
    Buffering,
    /// The execution exists but was never dispatched — no `work_runs`
    /// row was ever created for it (the execution was abandoned or
    /// orphaned before the scheduler could start it). No worker ran
    /// so no transcript was recorded.
    ///
    /// This is the graceful-degradation path for conflict-resolution
    /// and CI-fix revision executions that are abandoned when the
    /// spawning attempt retires before dispatch (T1291 pattern). The
    /// `execution_status` field carries the DB status for the error
    /// message so the caller can explain _why_ there is no transcript.
    NeverDispatched { execution_status: String },
    /// The id resolves to a `work_runs` row or `work_executions` row
    /// that has finished (or never recorded a transcript path).
    KnownNoTranscript,
    /// No `work_runs` row, no `work_executions` row, no live registry
    /// entry — the id is genuinely unknown to the engine.
    Unknown,
}

/// Resolve a transcript path for the `TailRunTranscript` verb.
///
/// `bossctl agents transcript` always passes
/// [`LiveWorkerState::run_id`], which aliases the *execution* id
/// (`exec_*`) — the spawn flow stamps `WorkItemBinding.execution_id`
/// onto the registry entry. The pre-fix handler called
/// `work_db.get_run(run_id)`, which joins against `work_runs.id`
/// (`run_*`), so every transcript tail for a live worker returned
/// `unknown run` even when `agents list` reported the same worker
/// as `working`. This mirrors the cross-namespace bug fixed on the
/// write side in PR #384 and on the [`TranscriptPathResolver`] read
/// side immediately after. The resolver here is the
/// `TailRunTranscript` analogue: it tries the cache first (the
/// dispatcher's hot path), then both DB namespaces, and finally falls
/// back to the live registry so a worker that's been registered but
/// hasn't yet emitted a transcript-bearing hook surfaces as
/// `Buffering` rather than `Unknown`.
pub(super) fn resolve_transcript_for_tail(server_state: &ServerState, run_id: &str) -> TranscriptResolution {
    // Hot path: the dispatcher's in-memory cache, keyed on the same
    // execution-id namespace the live registry uses. Populated by
    // every hook event that carries `transcript_path`, so once the
    // first transcript-bearing hook lands this resolves immediately
    // even if the SQL write hasn't completed yet.
    if let Some(transcript_path) = server_state.transcript_path_cache.get(run_id) {
        return TranscriptResolution::Found { transcript_path };
    }

    // Persisted path: try the `run_*` namespace, then the `exec_*`
    // namespace. Either may succeed depending on what the caller had
    // in hand. `bossctl` passes `exec_*`; programmatic callers may
    // pass `run_*`.
    let run_lookup = server_state.work_db.get_run(run_id).ok();
    if let Some(transcript_path) = run_lookup.as_ref().and_then(|run| run.transcript_path.clone()) {
        return TranscriptResolution::Found { transcript_path };
    }
    let exec_path = server_state
        .work_db
        .transcript_path_for_execution(run_id)
        .ok()
        .flatten();
    if let Some(transcript_path) = exec_path {
        return TranscriptResolution::Found { transcript_path };
    }

    // No path on either row. Decide between "known but no transcript",
    // "live worker still buffering", and "genuinely unknown".
    let run_known = run_lookup.is_some();
    let execution = server_state.work_db.get_execution(run_id).ok();
    let execution_known = execution.is_some();
    let is_live = server_state.live_worker_states.is_run_live(run_id);

    if is_live {
        return TranscriptResolution::Buffering;
    }
    if run_known || execution_known {
        // Distinguish between "execution was abandoned before a worker was
        // ever started" (no work_runs row) and "worker ran but path wasn't
        // recorded". The former is the T1291 pattern: a conflict-resolution
        // or CI-fix revision execution gets abandoned by
        // `reconcile_revision_execution` when the spawning attempt retires
        // before the scheduler can pick up the execution. In that case the
        // work_runs table has no row at all (current_run_id=null on the
        // task), so pointing the user at transcript tail is misleading —
        // instead surface a clear "never dispatched" message.
        if !run_known && execution_known {
            let has_any_run = server_state.work_db.has_run_row_for_execution(run_id).unwrap_or(true); // on DB error, default to "yes" → fall through to KnownNoTranscript
            if !has_any_run {
                let status = execution
                    .as_ref()
                    .map(|e| e.status.as_str().to_owned())
                    .unwrap_or_else(|| "unknown".to_owned());
                return TranscriptResolution::NeverDispatched {
                    execution_status: status,
                };
            }
        }
        return TranscriptResolution::KnownNoTranscript;
    }
    TranscriptResolution::Unknown
}

/// Convert an internal `TranscriptSegment` from the converter crate to the
/// wire-protocol `TranscriptSegment` that travels over the RPC socket.
pub(super) fn segment_to_wire(s: crate::transcript_markdown::TranscriptSegment) -> boss_protocol::TranscriptSegment {
    use crate::transcript_markdown::SegmentRole;
    boss_protocol::TranscriptSegment {
        seq: s.seq,
        role: match s.role {
            SegmentRole::User => boss_protocol::SegmentRole::User,
            SegmentRole::Assistant => boss_protocol::SegmentRole::Assistant,
            SegmentRole::Thinking => boss_protocol::SegmentRole::Thinking,
            SegmentRole::Tool => boss_protocol::SegmentRole::Tool,
            SegmentRole::System => boss_protocol::SegmentRole::System,
        },
        label: s.label,
        timestamp: s.timestamp,
        model: s.model,
        markdown: s.markdown,
        collapsible: s.collapsible,
        default_collapsed: s.default_collapsed,
        truncated: s.truncated.map(|t| boss_protocol::TruncationInfo {
            shown_bytes: t.shown_bytes,
            total_bytes: t.total_bytes,
        }),
    }
}

pub(super) async fn read_transcript_tail(transcript_path: &str, lines: usize) -> std::io::Result<(Vec<String>, bool)> {
    let contents = tokio::fs::read_to_string(transcript_path).await?;
    Ok(tail_lines_from_content(&contents, lines))
}

/// Split raw transcript text into its last `lines` JSONL lines, plus a
/// `truncated` flag (true when earlier lines were dropped). Shared by
/// the local read above and the remote-over-SSH pull
/// ([`crate::remote_transcript`]) so both transports produce an
/// identical `RunTranscriptTail` payload. `lines == 0` returns ALL lines
/// (the whole file, no truncation).
pub(super) fn tail_lines_from_content(contents: &str, lines: usize) -> (Vec<String>, bool) {
    let split_lines: Vec<&str> = contents.lines().collect();
    if lines == 0 {
        // 0 = "all lines" — return the complete transcript, never truncated.
        return (split_lines.into_iter().map(str::to_owned).collect(), false);
    }
    let total = split_lines.len();
    let take = lines.min(total);
    let truncated = total > take;
    let tail = split_lines.into_iter().skip(total - take).map(str::to_owned).collect();
    (tail, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── format_epoch_iso8601 ────────────────────────────────────────────────
    //
    // Expected strings are taken from an independent epoch->UTC converter,
    // NOT derived by re-reading the civil-date algorithm under test.

    #[test]
    fn format_epoch_unix_epoch_is_1970() {
        assert_eq!(format_epoch_iso8601(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn format_epoch_known_modern_instant() {
        // 1700000000 == 2023-11-14T22:13:20Z (a well-known round epoch).
        assert_eq!(format_epoch_iso8601(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn format_epoch_leap_day() {
        // 2024 is a leap year; 1709164800 is exactly midnight of Feb 29.
        // Also exercises the m <= 2 February year-adjustment branch.
        assert_eq!(format_epoch_iso8601(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn format_epoch_year_month_boundary() {
        // One second before 2024-01-01T00:00:00Z (1704067200) rolls the
        // year, month, and day all back to the final second of 2023.
        assert_eq!(format_epoch_iso8601(1_704_067_199), "2023-12-31T23:59:59Z");
        assert_eq!(format_epoch_iso8601(1_704_067_200), "2024-01-01T00:00:00Z");
    }

    #[test]
    fn format_epoch_january_year_adjust_branch() {
        // A January instant exercises the m <= 2 branch with a non-zero
        // time-of-day. 1736944245 == 2025-01-15T12:30:45Z.
        assert_eq!(format_epoch_iso8601(1_736_944_245), "2025-01-15T12:30:45Z");
    }

    #[test]
    fn format_epoch_negative_pre_1970_wraps_time_of_day() {
        // Negative epochs must use Euclidean div/rem so the time-of-day
        // wraps to a positive value rather than going negative. One second
        // before the epoch is 1969-12-31T23:59:59Z.
        assert_eq!(format_epoch_iso8601(-1), "1969-12-31T23:59:59Z");
        // A full day before the epoch is exactly midnight of 1969-12-31.
        assert_eq!(format_epoch_iso8601(-86_400), "1969-12-31T00:00:00Z");
    }

    // ── validate_external_tracker_config ────────────────────────────────────

    #[test]
    fn validate_tracker_valid_github_is_ok() {
        let config = serde_json::json!({
            "org": "acme",
            "repo": "widgets",
            "project_number": 7,
        });
        assert_eq!(validate_external_tracker_config("github", &config), Ok(()));
    }

    #[test]
    fn validate_tracker_missing_org() {
        let config = serde_json::json!({ "repo": "widgets", "project_number": 7 });
        assert_eq!(
            validate_external_tracker_config("github", &config),
            Err("missing required field 'org' for kind=github".to_owned()),
        );
    }

    #[test]
    fn validate_tracker_missing_repo() {
        let config = serde_json::json!({ "org": "acme", "project_number": 7 });
        assert_eq!(
            validate_external_tracker_config("github", &config),
            Err("missing required field 'repo' for kind=github".to_owned()),
        );
    }

    #[test]
    fn validate_tracker_empty_org_treated_as_missing() {
        let config = serde_json::json!({ "org": "", "repo": "widgets", "project_number": 7 });
        assert_eq!(
            validate_external_tracker_config("github", &config),
            Err("missing required field 'org' for kind=github".to_owned()),
        );
    }

    #[test]
    fn validate_tracker_empty_repo_treated_as_missing() {
        let config = serde_json::json!({ "org": "acme", "repo": "", "project_number": 7 });
        assert_eq!(
            validate_external_tracker_config("github", &config),
            Err("missing required field 'repo' for kind=github".to_owned()),
        );
    }

    #[test]
    fn validate_tracker_missing_project_number() {
        let config = serde_json::json!({ "org": "acme", "repo": "widgets" });
        assert_eq!(
            validate_external_tracker_config("github", &config),
            Err("missing required field 'project_number' for kind=github".to_owned()),
        );
    }

    #[test]
    fn validate_tracker_non_numeric_project_number() {
        let config = serde_json::json!({
            "org": "acme",
            "repo": "widgets",
            "project_number": "7",
        });
        assert_eq!(
            validate_external_tracker_config("github", &config),
            Err("'project_number' must be a number for kind=github".to_owned()),
        );
    }

    #[test]
    fn validate_tracker_unknown_kind() {
        let config = serde_json::json!({});
        assert_eq!(
            validate_external_tracker_config("jira", &config),
            Err("unknown tracker kind 'jira'; supported: github".to_owned()),
        );
    }

    // ── task_transitioned_to_active ─────────────────────────────────────────

    fn task_with_status(status: TaskStatus) -> WorkItem {
        let task = boss_protocol::Task::builder()
            .id("task_test")
            .product_id("prod_test")
            .kind(boss_protocol::TaskKind::Task)
            .name("Test task")
            .description("desc")
            .status(status)
            .autostart(false)
            .created_at("2026-01-01T00:00:00Z")
            .updated_at("2026-01-01T00:00:00Z")
            .build();
        WorkItem::Task(task)
    }

    fn non_task_work_item() -> WorkItem {
        let product = boss_protocol::Product::builder()
            .id("prod_test")
            .name("Test product")
            .slug("test-product")
            .description("")
            .status("active")
            .created_at("2026-01-01T00:00:00Z")
            .updated_at("2026-01-01T00:00:00Z")
            .build();
        WorkItem::Product(product)
    }

    #[test]
    fn transitioned_non_task_is_false() {
        // Even with a "from non-active" previous status, a Product never
        // counts as a task transition.
        assert!(!task_transitioned_to_active(
            &Some(TaskStatus::Blocked),
            &non_task_work_item(),
        ));
    }

    #[test]
    fn transitioned_status_not_active_is_false() {
        assert!(!task_transitioned_to_active(
            &Some(TaskStatus::Blocked),
            &task_with_status(TaskStatus::Blocked),
        ));
    }

    #[test]
    fn transitioned_previous_none_is_true() {
        // No prior row seen → treat as a real transition into active.
        assert!(task_transitioned_to_active(
            &None,
            &task_with_status(TaskStatus::Active)
        ));
    }

    #[test]
    fn transitioned_previous_active_is_false() {
        // Idempotent retry: active-on-active is not a transition.
        assert!(!task_transitioned_to_active(
            &Some(TaskStatus::Active),
            &task_with_status(TaskStatus::Active),
        ));
    }

    #[test]
    fn transitioned_previous_other_is_true() {
        assert!(task_transitioned_to_active(
            &Some(TaskStatus::Blocked),
            &task_with_status(TaskStatus::Active),
        ));
    }
}
