use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command as TokioCommand;
use tokio::sync::{Mutex, Notify, oneshot};

use crate::audit_effort;
use crate::cli::Cli;
use crate::completion::{
    CommandPrDetector, PaneReleaseOutcome, PrDetector, ProbeQueuer, WorkerCompletionHandler, WorkerPaneReleaser,
};
use crate::config::RuntimeConfig;
use crate::coordinator::{CommandCubeClient, CubeClient, ExecutionCoordinator, ExecutionPublisher, WorkerPool};
use crate::driver::AgentDriver;
use crate::events_socket::{bind_events_socket, handle_connection, peer_pid};
use crate::external_tracker::github_oauth::{
    DeviceFlow, GitHubAuthController, GitHubAuthState, KeychainTokenStore, probe_and_record_org_state,
};
use crate::ipc_log::IpcLogger;
use crate::live_status_loop::{LiveStatusBroadcaster, LiveStatusManager, TranscriptPathResolver, Trigger};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::merge_poller::{CommandMergeProbe, MergeProbe, spawn_loop as spawn_merge_poller};
use crate::merge_when_ready;
use crate::protocol::{
    EngineToAppError, EngineToAppRequest, EngineToAppResponse, FocusWorkerPaneInput, FrontendEvent,
    FrontendEventEnvelope, FrontendRequest, FrontendRequestEnvelope, GitHubAuthStateDto, InterruptWorkerPaneInput,
    OrgAuthState, ReleaseWorkerPaneInput, RequestExecutionInput, RevealWorkItemInput, SendToPaneInput,
    TOPIC_ENGINE_HEALTH, TOPIC_GITHUB_AUTH, TOPIC_WORK_PRODUCTS, TOPIC_WORKER_LIVE_STATES, TopicEventPayload,
    comment_topic, editorial_actions_topic, execution_topic, magic_wand_dispatch_topic, probe_topic,
    work_product_topic,
};
use crate::repo_slug;
use crate::work::{
    ActionedAttentionGroup, COMMENT_STATUS_DISPATCHED, CreateChoreInput, DuplicateTaskError, ExecutionStatus,
    GhPrStateChecker, SetRunTranscriptPathOutcome, Task, TaskStatus, WorkDb, WorkItem,
};
use crate::worker_registry::WorkerRegistry;
use async_trait::async_trait;
use tokio::time::{Duration, timeout};

mod attentions;
mod automations;
mod ci_remediation;
mod comments;
mod conflict_resolution;
mod dependencies;
mod effort;
mod engine_meta;
mod executions;
mod external_tracker;
mod github_auth;
mod handler_helpers;
mod hosts;
mod live_status;
mod metrics;
mod panes;
mod products;
mod projects;
mod review;
mod server;
mod sessions;
mod subscriptions;
#[cfg(test)]
mod tests;
mod work_items;
mod worker_events;

// Re-export public items from server module for external callers.
pub use server::{process_is_alive, run, serve};

// Re-import server-internal helpers so child modules can access them via `use super::*`.
use server::{
    constant_time_eq, is_descendant_of_any, reap_worker_process_tree, register_app_session_trust_ok,
    resolve_status_actor, signal_shell_pids,
};

// Re-import worker event dispatch functions so child modules can access them via `use super::*`.
use worker_events::{
    dispatch_completion_on_stop, dispatch_editorial_on_pretooluse, dispatch_live_worker_state, dispatch_probe_if_idle,
    dispatch_probe_on_stop, dispatch_probe_reply_on_stop, dispatch_urgent_probe_on_post_tool_use,
};

// Re-import handler helpers so all handler submodules can access them via `use super::*`.
use handler_helpers::{
    METADATA_KEY_DISPATCH_PAUSED, METADATA_KEY_DISPATCH_PAUSED_SINCE, TRANSCRIPT_NOT_YET_AVAILABLE_PREFIX,
    TranscriptResolution, active_chore_run_id, active_to_todo_execution, build_chore_update_message,
    build_effort_audit_report, build_engine_health_report, build_live_status_debug_report, duplicate_or_work_error,
    handle_create_many, in_review_chore_execution, live_execution_for_deleted_item, load_dispatch_paused_state,
    load_live_status_disabled_slots, open_review_terminal_async, persist_live_status_disabled_slots,
    publish_comment_invalidation, publish_work_invalidation, read_transcript_tail, resolve_transcript_for_tail,
    segment_to_wire, send_push, send_response, send_response_with_revision, tail_lines_from_content,
    task_name_description_for_id, task_status_for_id, task_transitioned_to_active, terminal_chore_execution,
    transport_default_created_via, validate_external_tracker_config, work_item_id, work_item_needs_dispatch,
    work_item_product_id,
};

/// Per-request handler context: the connection-scoped state every
/// [`FrontendRequest`] handler needs. Built once per request in
/// [`handle_frontend_connection`] and consumed by the dispatched handler.
/// Bundling these into one struct keeps the dispatch match a thin
/// alphabetical table of `Variant => module::handler(ctx, r)` arms so
/// concurrent PRs adding new requests don't all collide at the tail.
#[derive(bon::Builder)]
#[builder(on(String, into))]
struct Dispatch {
    server_state: Arc<ServerState>,
    work_db: Arc<WorkDb>,
    sink: Arc<SessionSink>,
    session_id: String,
    request_id: String,
    peer_pid: Option<libc::pid_t>,
}

const DEFAULT_SOCKET_PATH: &str = "/tmp/boss-engine.sock";
const DEFAULT_PID_PATH: &str = "/tmp/boss-engine.pid";

/// Shared HTTP client for the GitHub OAuth device flow. Installs the rustls
/// ring crypto provider lazily (the first TLS handshake panics otherwise,
/// mirroring `live_status::http_client`) and applies a per-request timeout —
/// the device-flow poll loop manages its own cadence, so this only bounds an
/// individual round-trip, never the overall flow.
fn github_oauth_http_client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("reqwest::Client build should not fail with default config")
}

#[async_trait]
impl LiveStatusBroadcaster for ServerState {
    async fn broadcast_live_worker_states(&self) {
        // Disambiguate against the trait method of the same name —
        // call the inherent publisher directly via UFCS so this
        // doesn't recurse.
        ServerState::broadcast_live_worker_states(self).await;
    }
}

#[async_trait]
impl TranscriptPathResolver for ServerState {
    async fn transcript_path(&self, run_id: &str) -> Option<std::path::PathBuf> {
        // The "run_id" the live-status manager hands us is actually the
        // execution id (`exec_*`) — `LiveWorkerState.run_id` is stamped
        // from `WorkItemBinding.execution_id` at spawn, and the rest of
        // the engine is consistent with that aliasing. The pre-fix
        // version of this resolver called `work_db.get_run(run_id)`
        // (which joins on `work_runs.id`, an `run_*` namespace), so the
        // lookup never matched and the per-slot summarizer never
        // resolved a transcript path. That blocked `tail` from ever
        // being instantiated, which in turn meant `snap.transcript_path`
        // was never populated in the debug store — visible to the user
        // as `bossctl live-status debug --json` reporting
        // `slots[*].transcript_path: null` for every live slot.
        //
        // PR #384 fixed the same cross-namespace bug on the write side
        // (`set_run_transcript_path_if_unset`). This is the read-side
        // pair. Keep both routed through helpers that explicitly take
        // an execution id so a future grep for `work_db.get_run` in this
        // file can stay a strong "this is the wrong namespace" signal.
        match self.work_db.transcript_path_for_execution(run_id) {
            Ok(Some(path)) => Some(std::path::PathBuf::from(path)),
            Ok(None) => None,
            Err(err) => {
                tracing::debug!(run_id, ?err, "live_status: transcript path lookup failed");
                None
            }
        }
    }
}

#[async_trait]
impl crate::spawn_flow::WorkerSpawner for ServerState {
    async fn send_to_app_request(
        &self,
        request: EngineToAppRequest,
        timeout: Duration,
    ) -> Result<EngineToAppResponse, SendToAppError> {
        // Serialize SpawnWorkerPane round-trips. Concurrent bursts of
        // surface_new on the macOS side crashed the app
        // (slot 4 spawned, then 3 follow-ups timed out into a dead
        // process). The app reasonably allocates panes one at a time,
        // and there's no benefit to dispatching parallel spawns —
        // gating the engine side keeps libghostty from being asked to
        // stand up multiple surfaces inside a single runloop tick.
        // ReleaseWorkerPane / SendToPane don't share this hazard, so
        // they go through unsynchronized.
        if matches!(request, EngineToAppRequest::SpawnWorkerPane(_)) {
            let _guard = self.spawn_pane_lock.lock().await;
            return self.send_to_app(request, timeout).await;
        }
        self.send_to_app(request, timeout).await
    }

    fn worker_registry(&self) -> &WorkerRegistry {
        &self.worker_registry
    }

    async fn reap_worker_pane(&self, run_id: &str) {
        // Delegate to the inherent reaper, discarding the outcome — the
        // spawn-completion path already knows the worker came up (it just
        // returned a pid); it calls this purely to kill it.
        let _ = ServerState::release_worker_pane(self, run_id).await;
    }

    fn live_worker_state_registry(&self) -> Option<&LiveWorkerStateRegistry> {
        Some(&self.live_worker_states)
    }

    async fn publish_live_worker_states(&self) {
        self.broadcast_live_worker_states().await;
    }

    fn start_live_status_slot(&self, slot_id: u8, run_id: &str) {
        let Some(arc_self) = self._self_weak.upgrade() else {
            tracing::debug!(slot_id, "start_live_status_slot: ServerState already dropped",);
            return;
        };
        // Snapshot the API key once at slot start — picking it up
        // lazily inside the task would require sharing the config or
        // a closure, and the key doesn't change for the worker's
        // lifetime anyway.
        let api_key = arc_self.anthropic_api_key.clone();
        let broadcaster: Arc<dyn LiveStatusBroadcaster> = arc_self.clone();
        let resolver: Arc<dyn TranscriptPathResolver> = arc_self.clone();
        self.live_status_manager.start_slot(
            slot_id,
            run_id.to_owned(),
            api_key,
            self.live_worker_states.clone(),
            broadcaster,
            resolver,
        );
    }

    fn draft_pr_mode(&self) -> bool {
        self.settings.is_enabled("default_pr_draft_mode")
    }

    fn non_opus_auto_mode(&self) -> bool {
        self.settings.is_enabled("workers.non_opus_permission_mode")
    }
}

#[async_trait]
impl crate::stale_worker_sweep::StaleWorkerReaper for ServerState {
    /// Route the stale-worker reconcile through the exact teardown
    /// `bossctl agents stop` performs: `release_worker_pane` tears down
    /// the libghostty pane, fires the `reap_worker_process_tree`
    /// SIGTERM/SIGKILL ladder at the worker's process group, releases the
    /// pool slot, and drops the live-state entry. This is what was
    /// missing — the sweep used to free the pool slot without ever
    /// killing the `claude` process, so a redispatch could re-lease the
    /// still-occupied workspace.
    async fn reap_worker(&self, execution_id: &str) {
        let _ = ServerState::release_worker_pane(self, execution_id).await;
    }
}

/// `WorkerPaneReleaser` implementation backed by a `Weak<ServerState>`.
/// Late-bound via `set_server_state` to break the ownership cycle:
/// ServerState owns the completion handler, which owns the releaser,
/// which calls back into ServerState.
#[derive(Default)]
struct ServerStatePaneReleaser {
    server: std::sync::OnceLock<Weak<ServerState>>,
}

impl ServerStatePaneReleaser {
    fn set_server_state(&self, weak: Weak<ServerState>) {
        let _ = self.server.set(weak);
    }
}

#[async_trait]
impl WorkerPaneReleaser for ServerStatePaneReleaser {
    async fn release_pane(&self, run_id: &str) -> PaneReleaseOutcome {
        let Some(weak) = self.server.get() else {
            tracing::warn!(run_id, "pane releaser called before server state was bound");
            // No server bound: nothing could be reaped. Treat as
            // "no live worker" so the caller does not free a lease on
            // the strength of a release that never happened.
            return PaneReleaseOutcome::NoLiveWorker;
        };
        let Some(server) = weak.upgrade() else {
            tracing::debug!(run_id, "pane releaser: server state already dropped");
            return PaneReleaseOutcome::NoLiveWorker;
        };
        server.release_worker_pane(run_id).await
    }
}

/// Adapter so the completion handler can queue probes onto
/// `ServerState::pending_probes` without depending on `ServerState`
/// directly. Same late-bind dance as `ServerStatePaneReleaser` — the
/// completion handler is built before the `Arc<ServerState>` exists,
/// then `set_server_state` plumbs the upgrade target in. The next
/// `Stop` event for the run pops one queued entry and `SendToPane`s
/// it as if the user had typed it (`dispatch_probe_on_stop`).
#[derive(Default)]
struct ServerStateProbeQueuer {
    server: std::sync::OnceLock<Weak<ServerState>>,
}

impl ServerStateProbeQueuer {
    fn set_server_state(&self, weak: Weak<ServerState>) {
        let _ = self.server.set(weak);
    }
}

impl ProbeQueuer for ServerStateProbeQueuer {
    fn queue_probe(&self, run_id: &str, text: &str) {
        let Some(weak) = self.server.get() else {
            tracing::warn!(run_id, "probe queuer called before server state was bound");
            return;
        };
        let Some(server) = weak.upgrade() else {
            tracing::debug!(run_id, "probe queuer: server state already dropped");
            return;
        };
        // Completion-driven probes don't need the minted id — only
        // the human-driven `ProbeRun` RPC surfaces it back to the
        // caller. Discard it here. Completion probes are never urgent.
        let _ = server.queue_probe(run_id.to_owned(), text.to_owned(), false);
    }
}

/// One queued probe that has not yet been dispatched into the worker.
#[derive(Debug, Clone)]
struct PendingProbe {
    probe_id: String,
    text: String,
    /// When `true`, dispatch at the next `PostToolUse` boundary
    /// rather than waiting for the next `Stop`. Urgent probes are
    /// always inserted at the front of the per-run queue.
    urgent: bool,
}

/// One probe that has been written into the worker's pane and is
/// waiting for the next `Stop` boundary so we can emit
/// `FrontendEvent::ProbeReplied` with the assistant turn that
/// landed in the transcript afterwards.
#[derive(Debug, Clone)]
struct InFlightProbe {
    probe_id: String,
    /// Transcript path captured at dispatch time. Stashing it here
    /// (rather than re-querying `WorkRun` on the follow-up Stop)
    /// keeps reply extraction tied to the file the worker was
    /// actually writing when the probe landed, even if the run row
    /// is later updated to point elsewhere.
    transcript_path: Option<String>,
    /// Bytes-on-disk size of the transcript at dispatch time. The
    /// follow-up Stop reads `[offset_bytes..len]` and parses each
    /// new JSONL line — anything earlier already pre-dated the probe
    /// and isn't part of the reply.
    offset_bytes: u64,
}

struct PidFileGuard {
    path: String,
    pid: u32,
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(_) => return,
        };

        let parsed = content.trim().parse::<u32>().ok();
        if parsed == Some(self.pid) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(bon::Builder)]
#[builder(on(String, into))]
struct ServerState {
    work_db: Arc<WorkDb>,
    execution_coordinator: Arc<ExecutionCoordinator>,
    completion_handler: Arc<WorkerCompletionHandler>,
    /// Direct handle to the cube client, used by control verbs that
    /// don't otherwise go through the execution coordinator (e.g.
    /// `WorkspacePoolSummary`).
    cube_client: Arc<dyn CubeClient>,
    /// Shared event publisher. The execution coordinator and
    /// completion handler each hold their own `Arc` clones; this
    /// field exists so background tasks spawned out of `Self::new`
    /// (the merge poller, etc.) can publish work-item invalidations
    /// without standing up a second broker.
    publisher: Arc<dyn ExecutionPublisher>,
    /// Shared dispatch-event sink. The execution coordinator emits
    /// the per-stage events into this sink during dispatch; the
    /// `UpdateWorkItem` handler emits a `StatusTransition` event
    /// before dispatch even gets a chance to fire, which is the
    /// only signal we have when the auto-dispatch gate decides to
    /// skip (the "I dragged it and nothing happened" symptom).
    dispatch_events: Arc<dyn crate::dispatch_events::DispatchEventSink>,
    /// Root path the dispatch-event sink writes under. Surfaced on
    /// `ServerState` so the stage-stalled detector (spawned out of
    /// `serve`) can run [`crate::dispatch_reader::pending_stalls`]
    /// against the same files the sink populates.
    dispatch_event_root: PathBuf,
    topic_broker: Arc<TopicBroker>,
    worker_registry: WorkerRegistry,
    /// Live runtime state per allocated worker slot. Updated as hook
    /// events arrive on the events socket; surfaced to bossctl/UI via
    /// `ListWorkerLiveStates` and pushed on the
    /// `worker.live_states` topic whenever any slot changes.
    live_worker_states: Arc<LiveWorkerStateRegistry>,
    /// Per-slot trigger fan-in for the live-status summarizer. Started
    /// when `spawn_flow` calls `start_live_status_slot`; torn down
    /// in `release_worker_pane`.
    live_status_manager: Arc<LiveStatusManager>,
    /// Engine-wide counters for the hook-event dispatcher. Surfaced
    /// by the `bossctl live-status debug` verb so an operator can
    /// see at a glance whether hooks are arriving, whether their
    /// payloads carry `transcript_path`, and whether the persist
    /// call into `work_runs` succeeded. Added as the visibility
    /// surface that PR #366 did not have — without it, a stalled
    /// pipeline looked indistinguishable from a healthy one.
    dispatcher_stats: Arc<crate::live_status_loop::DispatcherStats>,
    /// Per-run in-memory `transcript_path` cache. The dispatcher
    /// populates this whenever a hook payload carries the field and
    /// uses it as a fallback whenever a subsequent hook for the same
    /// run lacks the field. See [`TranscriptPathCache`] for why this
    /// is the structural fix for the 2026-05-12 incident.
    transcript_path_cache: Arc<crate::live_status_loop::TranscriptPathCache>,
    /// Primary-path `execution_id → pr_url` staging cache. Populated
    /// by [`dispatch_live_worker_state`] from `PostToolUse` Bash
    /// hooks that surface a `gh pr create` (or `view` / `edit`)
    /// URL in `tool_response.stdout`. Read by
    /// [`WorkerCompletionHandler::on_stop`] (and `recheck_for_pr`)
    /// on the matching Stop to skip the `jj log` + `gh api` PR
    /// reconstruction entirely.
    ///
    /// Shared with the completion handler via
    /// [`WorkerCompletionHandler::with_staged_pr_urls`] so writes
    /// here and reads in `on_stop` see the same map.
    staged_pr_urls: Arc<crate::pr_url_capture::StagedPrUrlCache>,
    /// Per-execution deny counter for the editorial PreToolUse loop guard
    /// (design R3). State is in-memory only; a restart resets it to zero,
    /// which is the safe direction (worst case a worker gets three fresh
    /// denies rather than an indefinite block).
    editorial_deny_tracker: Arc<crate::editorial_hook::DenyTracker>,
    /// Snapshot of the Anthropic API key captured at engine startup.
    /// Used by the live-status summarizer for the per-slot task; the
    /// pane-titlebar summarizer continues to resolve the key
    /// per-spawn via `cfg.agent()`.
    anthropic_api_key: Option<String>,
    /// Live pool sizes clamped at engine startup. Pushed to the macOS
    /// app as `EnginePoolConfig` on every `RegisterAppSession` so the
    /// app's `WorkersWorkspaceModel` slot ranges always mirror the
    /// engine's actual allocation limits, not independently-maintained
    /// constants that drift when pool sizes change.
    worker_pool_size: u8,
    automation_pool_size: u8,
    review_pool_size: u8,
    /// Shared verdict from the `syspolicyd` CPU monitor. The sampler loop
    /// (spawned in `serve`) writes it; [`build_engine_health_report`]
    /// reads it to raise a banner when the daemon wedges and stalls all
    /// builds. See [`crate::syspolicyd_monitor`].
    syspolicyd_health: Arc<crate::syspolicyd_monitor::SyspolicydHealth>,
    next_session_id: AtomicU64,
    work_revision: Arc<AtomicU64>,
    /// Pid of the process the engine trusts as the macOS app — must
    /// match a session's `peer_pid` for `RegisterAppSession` to
    /// succeed. `None` only in tests; production seeds this from
    /// `BOSS_APP_PID` at startup.
    ///
    /// Interior-mutable because the app can restart against a surviving
    /// engine (same-version relaunch — the engine correctly stays up).
    /// The relaunched app has a new pid, so the trust root must be
    /// re-pinned to it on re-registration; otherwise the stale pid
    /// rejects every `RegisterAppSession` and engine→app RPCs
    /// (`SpawnWorkerPane`, reveal) die. See `register_app_session`'s
    /// caller and `current_app_pid`/`set_app_pid`.
    app_pid: StdMutex<Option<libc::pid_t>>,
    /// Pid of the Boss session's shell, set by the app via
    /// `RegisterBossSession` once the Boss libghostty pane has spawned.
    /// Used as the second trust root: a peer whose process tree
    /// includes this pid as an ancestor is treated as the Boss tier
    /// for RPC authorization.
    boss_pid: StdMutex<Option<libc::pid_t>>,
    /// Pending probes per run, FIFO. Each entry is the engine-minted
    /// `probe_id` paired with the verbatim text the caller queued.
    /// The events-socket consumer pops one entry per `Stop` hook event
    /// for the matching run and dispatches it as `SendToPane` to the
    /// app.
    pending_probes: StdMutex<HashMap<String, VecDeque<PendingProbe>>>,
    /// Probes that have been dispatched into a worker pane and are
    /// awaiting the *next* `Stop` boundary so the engine can extract
    /// the worker's reply from its transcript and emit
    /// `FrontendEvent::ProbeReplied`. One entry per run at most — the
    /// next Stop after dispatch consumes it. The transcript byte
    /// offset captured at dispatch time bounds the read, so we don't
    /// re-emit text that pre-dated the probe.
    in_flight_probes: StdMutex<HashMap<String, InFlightProbe>>,
    /// Monotonic counter used to mint probe ids (`probe-{n}`). Probe
    /// ids only need to be unique for the lifetime of one engine
    /// process — they correlate a `ProbeRun` request with its
    /// follow-up `ProbeReplied` push, and clients don't persist them.
    next_probe_id: AtomicU64,
    /// Currently-registered app session, if any. Engine→app requests
    /// are routed only to this session.
    app_session: Arc<Mutex<Option<AppSessionHandle>>>,
    /// Serializes outbound `SpawnWorkerPane` round-trips so the app
    /// only ever sees one pane allocation in flight at a time. See the
    /// `WorkerSpawner` impl for the why.
    spawn_pane_lock: Arc<Mutex<()>>,
    /// Append-only JSONL log of every engine↔app IPC exchange. Each
    /// `send_to_app` call appends an `engine→app` record; each
    /// `deliver_app_response` call appends an `app→engine` record.
    /// Backed by a background task so log writes never block the hot
    /// path. Files rotate daily under `<state-root>/ipc/`.
    ipc_logger: IpcLogger,
    /// Weak self-reference produced by `Arc::new_cyclic`. Kept so
    /// late-bound consumers (the pane-spawn runner) can resolve back
    /// to the live `Arc<ServerState>` without an outer allocation.
    _self_weak: Weak<ServerState>,
    /// Toggleable feature flags for optional/risk-bearing engine
    /// behaviours (incident 001 AI #5). Loaded from
    /// `~/Library/Application Support/Boss/feature-flags.toml` at
    /// boot, mutated by `SetFeatureFlag` RPC, consulted by callers
    /// via `is_enabled(...)`. See `crate::feature_flags`.
    feature_flags: Arc<crate::feature_flags::FeatureFlagsStore>,
    /// Registry of capability IDs present in the current running
    /// build. Populated by `RegisterCapabilities` RPC when the macOS
    /// app connects, and by engine-side startup for engine-built
    /// features. Consulted by `snapshot_all` to populate
    /// `capability_present` on every `FeatureFlagSnapshot`.
    capability_registry: Arc<crate::feature_flags::CapabilityRegistry>,
    /// Per-installation settings (e.g. default_pr_draft_mode). Loaded
    /// from `~/Library/Application Support/Boss/settings.toml` at boot,
    /// mutated by `SetSetting` RPC, consulted by the spawn flow to
    /// inject worker directives. See `crate::settings`.
    settings: Arc<crate::settings::SettingsStore>,
    /// Engine-wide counter / gauge registry. Plumbed as an
    /// `Arc<Registry>` per the framework design's recommendation
    /// against globals (see
    /// `tools/boss/docs/designs/engine-counter-metrics-framework.md`
    /// §"Risks / open questions" item 7) — every call site that
    /// increments a counter takes a `&Registry`, which keeps
    /// counter state isolated per `ServerState` instance and
    /// makes unit tests cheap.
    metrics: Arc<crate::metrics::Registry>,
    /// Registry of external-tracker backends. Holds the `GitHubTracker`
    /// at startup; future backends (Jira, Linear) are registered the
    /// same way. Shared between the periodic spawn loop and the
    /// on-demand `SyncProductExternalTracker` handler.
    tracker_registry: Arc<crate::external_tracker::TrackerRegistry>,
    /// Single per-host (github.com) GitHub OAuth device-flow controller.
    /// Owns the auth state machine, the poll loop, and keychain persistence.
    /// The `GitHubAuthStart/Cancel/Disconnect/Status` handlers drive it; a
    /// forwarder task spawned in `serve` watches its state and pushes
    /// [`FrontendEvent::GitHubAuthState`] on [`TOPIC_GITHUB_AUTH`] plus runs
    /// the org/SSO probe. See the OAuth device-flow design (§3, §4, §7).
    github_auth: Arc<GitHubAuthController>,
    /// Resolves credentials for external-tracker sync. Uses
    /// `KeychainOAuthResolver` in production so a stored OAuth token
    /// takes precedence over ambient `gh` auth.
    tracker_credential_resolver: Arc<dyn crate::external_tracker::credentials::TrackerCredentialResolver>,
    /// Shared kick signal for the merge-poller loop. The macOS app
    /// fires [`FrontendRequest::KickPrReconcilers`] on window
    /// activation; the handler calls `notify_one()` here so the
    /// poller's next wait arm resolves immediately (subject to the
    /// 15 s engine-side quiesce window). `None` only between
    /// `new_arc` return and the first `spawn_merge_poller` call in
    /// `serve` — that window is < 1 ms in production.
    pr_reconciler_kick: Arc<Notify>,
    /// Kick signal for the automation scheduler loop. Notified by any
    /// automation mutation handler (create, update, enable, disable,
    /// delete) so the scheduler recomputes its min-next-fire sleep
    /// immediately on state change rather than waiting out its current
    /// interval. See [`crate::automation_scheduler::spawn_loop`].
    automation_scheduler_kick: Arc<Notify>,
    /// Secret token written to the control-token file at startup. A
    /// frontend `Shutdown { token }` RPC must match this value to
    /// trigger graceful exit. `None` only in tests / in-process
    /// `serve` calls that didn't ask for a control token — those
    /// callers can't shut the engine down over the wire (they always
    /// have direct ownership of the runtime handle and can drop it).
    control_token: Option<Arc<String>>,
    /// Notified by the `Shutdown` RPC handler after a successful token
    /// match. The accept loop in `serve` selects on this alongside the
    /// SIGTERM-style shutdown signal and exits the same graceful path
    /// when either fires.
    shutdown_trigger: Arc<Notify>,
}

/// Authorization tier for a frontend RPC.
///
/// - `User`: any local client (the human's `boss` CLI, the macOS app,
///   read-only callers, and any documented `bossctl` verb that has no
///   privileged side effect — e.g. `workspace summary`).
/// - `AppOrBoss`: privileged operations the app and the Boss session
///   may both invoke. This is the right level for the imperative
///   `bossctl` verbs (`probe`, `agents stop`, `agents transcript`,
///   `work cancel`): the human runs them from wherever they happen
///   to be — Boss pane, app shell, *inside a worker pane*, or a
///   plain terminal that descends from neither trust root. The
///   admission rule is "descendant of app or Boss, OR not a
///   descendant of any registered worker pane" — workers are the
///   only sibling-process adversary in the V2 threat model, so
///   excluding worker subtrees is sufficient. Earlier revisions
///   gated strictly on app/Boss subtree membership and locked the
///   coordinator out whenever it ran from a shell outside both
///   (e.g. a tmux pane started before the app launched).
/// - `BossOnly`: reserved for future control verbs that must reject
///   worker-pane callers. No live verb uses this tier today; the
///   `bossctl` verbs that previously gated on it (`probe_run`,
///   `tail_run_transcript`, `stop_run`) were all downgraded after
///   they kept locking the coordinator out of legitimate calls. Keep
///   the tier so any future verb can opt into it explicitly rather
///   than accidentally inheriting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcTier {
    User,
    AppOrBoss,
    BossOnly,
}

/// Live state for the registered app session. The sink is used to
/// push `EngineRequest` events; the pending map keys outstanding
/// engine→app calls by their `request_id`.
struct AppSessionHandle {
    session_id: String,
    sink: Arc<SessionSink>,
    pending: HashMap<String, oneshot::Sender<EngineToAppResponse>>,
    next_request_id: u64,
}

impl AppSessionHandle {
    fn new(session_id: String, sink: Arc<SessionSink>) -> Self {
        Self {
            session_id,
            sink,
            pending: HashMap::new(),
            next_request_id: 1,
        }
    }

    fn allocate_request_id(&mut self) -> String {
        let id = format!("eng-req-{}", self.next_request_id);
        self.next_request_id += 1;
        id
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum SendToAppError {
    #[error("no app session is registered")]
    NotRegistered,
    #[error("app disconnected before responding")]
    AppDisconnected,
    #[error("timed out waiting for app response")]
    Timeout,
    #[error("app responded with unexpected response kind for request kind {0}")]
    ResponseKindMismatch(&'static str),
}

/// Surfaced by [`ServerState::focus_worker_pane`]. Distinguishes
/// engine-side resolution failures (run id has no allocated slot)
/// from transport/app failures so the `bossctl` handler can produce
/// a precise error message.
#[derive(Debug, thiserror::Error)]
pub enum FocusPaneError {
    #[error("no worker pane mapped for that run id")]
    UnknownRun,
    #[error("app reported error: {0:?}")]
    App(EngineToAppError),
    #[error(transparent)]
    Send(#[from] SendToAppError),
    #[error("app returned unexpected response: {0}")]
    ResponseKindMismatch(String),
}

/// Surfaced by [`ServerState::send_input_to_worker`]. Same shape as
/// [`FocusPaneError`]: separates "no slot mapping for that run id"
/// from app-side / transport failures so `bossctl agents send` can
/// produce a precise error message.
#[derive(Debug, thiserror::Error)]
pub enum SendInputError {
    #[error("no worker pane mapped for that run id")]
    UnknownRun,
    #[error("app reported error: {0:?}")]
    App(EngineToAppError),
    #[error(transparent)]
    Send(#[from] SendToAppError),
    #[error("app returned unexpected response: {0}")]
    ResponseKindMismatch(String),
}

/// Surfaced by [`ServerState::interrupt_worker_pane`]. Mirrors
/// [`FocusPaneError`] — the same error tiers apply (resolution miss,
/// app failure, transport, response shape).
#[derive(Debug, thiserror::Error)]
pub enum InterruptPaneError {
    #[error("no worker pane mapped for that run id")]
    UnknownRun,
    #[error("app reported error: {0:?}")]
    App(EngineToAppError),
    #[error(transparent)]
    Send(#[from] SendToAppError),
    #[error("app returned unexpected response: {0}")]
    ResponseKindMismatch(String),
}

/// Surfaced by [`ServerState::reveal_work_item`]. Separates
/// id-resolution failures from app-side / transport failures so
/// `bossctl reveal` can produce a precise error.
#[derive(Debug, thiserror::Error)]
pub enum RevealItemError {
    #[error("no work item found for id: {0}")]
    NotFound(String),
    #[error("work item {0} is deleted")]
    Deleted(String),
    #[error("app reported error: {0:?}")]
    App(EngineToAppError),
    #[error(transparent)]
    Send(#[from] SendToAppError),
    #[error("app returned unexpected response: {0}")]
    ResponseKindMismatch(String),
}

impl ServerState {
    fn new_arc_with_app_pid(
        cfg: Arc<RuntimeConfig>,
        app_pid: Option<libc::pid_t>,
        control_token: Option<Arc<String>>,
    ) -> Result<Arc<Self>> {
        let work_db = Arc::new(WorkDb::open(cfg.work.db_path.clone())?);
        let anthropic_api_key = cfg.agent().ok().and_then(|agent| agent.anthropic_api_key.clone());
        // One-time startup signal so the missing-API-key case is
        // immediately visible in engine stderr — the chore calls out
        // that the summarizer used to drop this silently and the user
        // wants to confirm it's not the failure mode they're hitting.
        // Logged at `info` for the happy path so a `grep "live_status:"`
        // sweep still shows the engine made a decision.
        if anthropic_api_key.is_some() {
            tracing::info!("live_status: ANTHROPIC_API_KEY is configured; summarizer enabled",);
        } else {
            tracing::error!(
                "live_status: ANTHROPIC_API_KEY is NOT configured — \
                 every summarizer call will return no_api_key and no \
                 worker will get a live_status sentence. Set it in the \
                 engine's agent config or via env to enable.",
            );
        }
        // Engine build identity, logged once at startup so the user
        // can grep `live_status:` and confirm which binary is live.
        //
        // `build_info::init()` here is load-bearing: it pins the
        // binary fingerprint to the engine's on-disk bytes *as they
        // exist right now*, before any installer can replace the file
        // out from under us. Without it, the OnceLock would populate
        // on the first GetEngineVersion query, hashing whatever bytes
        // happen to be on disk at that moment — and if Boss.app was
        // updated while the engine was still running, those are the
        // *new* bytes. The macOS app would see "fingerprint matches
        // bundled engine" and silently attach to the stale engine
        // instead of triggering the version-mismatch restart from
        // T460. See `build_info::binary_fingerprint` doc comment.
        crate::build_info::init();
        tracing::info!(
            engine_build_sha = crate::build_info::git_sha(),
            engine_build_time = crate::build_info::build_time(),
            engine_binary_fingerprint = crate::build_info::binary_fingerprint(),
            "live_status: engine starting (build identity)",
        );
        // Phase 3 of distributed-agent-execution: sweep stale
        // OpenSSH ControlMaster sockets left behind by a previous
        // engine run that crashed before `SshTransport::close`. Per
        // the design's "Risks and Open Questions": this sweep is
        // non-negotiable — without it, a stale socket file can
        // prevent the next dispatch from binding a fresh master.
        if let Some(dir) = crate::ssh_transport::default_control_socket_dir() {
            match crate::ssh_transport::sweep_stale_control_sockets(&dir) {
                Ok(n) if n > 0 => {
                    tracing::info!(
                        swept = n,
                        dir = %dir.display(),
                        "engine startup: swept stale ssh control sockets",
                    );
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        dir = %dir.display(),
                        "engine startup: ssh control-socket sweep failed (non-fatal)",
                    );
                }
            }
        }
        let worker_pool = WorkerPool::new(cfg.work.worker_pool_size);
        let automation_pool = WorkerPool::new_automation(cfg.work.automation_pool_size);
        let review_pool = WorkerPool::new_review(cfg.work.review_pool_size);
        // Capture clamped pool sizes before they move into the coordinator so
        // we can embed them into ServerState and push EnginePoolConfig to the
        // macOS app on every RegisterAppSession.
        let worker_pool_size = cfg.work.worker_pool_size as u8;
        let automation_pool_size = cfg.work.automation_pool_size as u8;
        let review_pool_size = cfg.work.review_pool_size as u8;
        let topic_broker = Arc::new(TopicBroker::default());
        let work_revision = Arc::new(AtomicU64::new(0));
        let publisher_impl = Arc::new(BrokerExecutionPublisher {
            topic_broker: topic_broker.clone(),
            work_revision: work_revision.clone(),
            kick: std::sync::OnceLock::new(),
        });
        let publisher: Arc<dyn ExecutionPublisher> = publisher_impl.clone();
        let cube_client: Arc<dyn CubeClient> = Arc::new(CommandCubeClient::new(cfg.clone()));
        let pr_detector: Arc<dyn PrDetector> = Arc::new(CommandPrDetector::new());
        // The pane releaser and probe queuer both need a Weak<ServerState>
        // to call back into ServerState methods, so they're late-bound
        // after the Arc<ServerState> exists. Same pattern as
        // `PaneSpawnRunner` below.
        let pane_releaser = Arc::new(ServerStatePaneReleaser::default());
        let probe_queuer = Arc::new(ServerStateProbeQueuer::default());
        let staged_pr_urls = Arc::new(crate::pr_url_capture::StagedPrUrlCache::new());

        // Resolve the Boss state root early — both the feature-flags
        // store (loaded below, before the completion handler is
        // built) and the dispatch-event sink (set up further down)
        // land next to `state.db` under the same root. Empty parent
        // (test configs with `:memory:` for the DB path) falls back
        // to `cwd` so test artifacts stay co-located.
        let state_root: PathBuf = cfg
            .work
            .db_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| cfg.work.cwd.clone());

        // Load the feature-flags store from the on-disk file. A
        // missing or unreadable file is logged but does not block
        // startup: the in-memory store falls back to registry defaults
        // for every flag, which is the same behaviour as a fresh
        // install. Persisting failures inside `set` are caught by
        // the RPC handler.
        let feature_flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            crate::feature_flags::FeatureFlagsStore::default_path(&state_root),
        ));
        if let Err(err) = feature_flags.load() {
            tracing::warn!(
                ?err,
                path = %feature_flags.path().display(),
                "feature-flags: load failed; falling back to registry defaults",
            );
        }
        let feature_flags_for_handler = feature_flags.clone();
        let feature_flags_for_state = feature_flags.clone();

        // Load per-installation settings. Same boot contract as feature
        // flags: a missing or unreadable file falls back to registry
        // defaults; parse failures are logged but don't block startup.
        let settings = Arc::new(crate::settings::SettingsStore::new(
            crate::settings::SettingsStore::default_path(&state_root),
        ));
        if let Err(err) = settings.load() {
            tracing::warn!(
                ?err,
                path = %settings.path().display(),
                "settings: load failed; falling back to registry defaults",
            );
        }
        // Log active (non-default) settings at startup so the operator
        // can diagnose unexpected worker behaviour (e.g. draft PRs).
        for snap in settings.snapshot_all() {
            if snap.enabled != snap.default_enabled {
                tracing::info!(
                    key = %snap.key,
                    enabled = snap.enabled,
                    "settings: active non-default setting at startup",
                );
            }
        }
        let settings_for_state = settings.clone();

        // Engine counter-metrics registry. Built up front so it can
        // be cloned into ServerState; the registry is plumbed
        // explicitly rather than stashed in a global per the
        // framework design. `init_all` runs further down once the
        // Arc<ServerState> is in hand so a duplicate registration
        // panics during this boot path instead of inside the first
        // increment.
        let metrics_registry = Arc::new(crate::metrics::Registry::new());
        let metrics_for_state = metrics_registry.clone();
        let metrics_for_dispatcher = metrics_registry.clone();
        let metrics_for_completion = metrics_registry.clone();
        let metrics_for_coordinator = metrics_registry.clone();
        let pr_reconciler_kick = Arc::new(Notify::new());
        let pr_reconciler_kick_for_state = pr_reconciler_kick.clone();
        let automation_scheduler_kick = Arc::new(Notify::new());
        let automation_scheduler_kick_for_state = automation_scheduler_kick.clone();
        let shutdown_trigger = Arc::new(Notify::new());
        let shutdown_trigger_for_state = shutdown_trigger.clone();
        let control_token_for_state = control_token.clone();

        let mut tracker_registry = crate::external_tracker::TrackerRegistry::new();
        tracker_registry
            .register(Arc::new(crate::external_tracker::github::GitHubTracker::new()))
            .expect("github tracker is the only registered kind; duplicate is impossible");
        let tracker_registry = Arc::new(tracker_registry);
        let tracker_registry_for_state = tracker_registry.clone();

        // GitHub OAuth device-flow controller (single per-host: github.com).
        // Backed by the OS keychain; the forwarder spawned in `serve` restores
        // any persisted token at boot and pushes state transitions to the app.
        let (github_auth_controller, _github_auth_rx) = GitHubAuthController::with_store(
            DeviceFlow::production(github_oauth_http_client()),
            Arc::new(KeychainTokenStore::new()),
        );
        let github_auth_for_state = Arc::new(github_auth_controller);

        let tracker_credential_resolver: Arc<dyn crate::external_tracker::credentials::TrackerCredentialResolver> =
            Arc::new(crate::external_tracker::credentials::KeychainOAuthResolver::new(
                crate::external_tracker::github_oauth::KeychainTokenStore::new(),
            ));
        let ci_probe: Arc<dyn MergeProbe> = Arc::new(CommandMergeProbe::new());
        let completion_handler = Arc::new(
            WorkerCompletionHandler::new(
                work_db.clone(),
                pr_detector,
                cube_client.clone(),
                publisher.clone(),
                pane_releaser.clone(),
                probe_queuer.clone(),
            )
            .with_staged_pr_urls(staged_pr_urls.clone())
            .with_feature_flags(feature_flags_for_handler)
            .with_merge_probe(ci_probe)
            .with_metrics(metrics_for_completion)
            .with_max_review_cycles(cfg.work.max_review_cycles)
            .with_min_review_changed_lines(cfg.work.min_review_changed_lines),
        );

        // Build PaneSpawnRunner up front, hand its Weak<ServerState>
        // pointer back via set_server_state once the Arc exists. The
        // runner needs to call into ServerState (send_to_app +
        // worker_registry) while ServerState owns the runner —
        // Arc::new_cyclic breaks the cycle.
        let pane_runner = Arc::new(crate::runner::PaneSpawnRunner::new(
            cfg.clone(),
            work_db.clone(),
            feature_flags.clone(),
        ));
        let runner_for_coordinator = pane_runner.clone();
        let cube_client_for_state = cube_client.clone();
        let publisher_for_state = publisher.clone();

        // Dispatch-event JSONL stream lands next to state.db /
        // events.sock under the same `state_root` resolved above.
        let dispatch_event_root: PathBuf = state_root.clone();
        let dispatch_events: Arc<dyn crate::dispatch_events::DispatchEventSink> =
            Arc::new(crate::dispatch_events::JsonlFileSink::new(dispatch_event_root.clone()));
        let dispatch_events_for_state = dispatch_events.clone();
        let dispatch_event_root_for_state = dispatch_event_root.clone();
        let ipc_logger = IpcLogger::new(&dispatch_event_root);

        let completion_handler_for_coordinator = completion_handler.clone();
        // Distributed-execution PR3 inputs for the SSH-capable host-adapter
        // provider: the engine's local events socket (target of each remote
        // run's reverse `ssh -R` forward), the engine-owned control-socket
        // dir, and a config handle. Resolved out here so the move-closure
        // below can consume them.
        let cfg_for_provider = cfg.clone();
        let provider_events_socket = crate::runner::engine_events_socket_path();
        let provider_control_dir = crate::ssh_transport::default_control_socket_dir();
        // Create the live per-slot worker registry up front so the
        // coordinator's lease-time occupancy guard (defect 3) and
        // ServerState share the SAME registry instance.
        let live_worker_states = Arc::new(LiveWorkerStateRegistry::new());
        let live_worker_states_for_coordinator = live_worker_states.clone();
        let server_state = Arc::new_cyclic(move |weak_self: &Weak<ServerState>| {
            let mut execution_coordinator_inner = ExecutionCoordinator::with_publisher(
                work_db.clone(),
                worker_pool,
                cube_client,
                runner_for_coordinator,
                publisher,
            );
            execution_coordinator_inner.set_dispatch_events(dispatch_events);
            execution_coordinator_inner.set_metrics(metrics_for_coordinator);
            execution_coordinator_inner.set_live_worker_states(live_worker_states_for_coordinator);
            execution_coordinator_inner.set_automation_pool(automation_pool);
            execution_coordinator_inner.set_review_pool(review_pool);
            // Wire the SHA-delta gate's run-start snapshot: when an
            // execution transitions to `running`, the completion
            // handler captures the bound chore PR's head SHA into
            // `work_executions.pr_head_before`.
            execution_coordinator_inner.set_execution_started_hook(completion_handler_for_coordinator.clone());
            // Install the SSH-capable provider so the dispatch loop can
            // build a per-host adapter (local vs SSH-remote) for whichever
            // host the scheduler selects. `local` returns the coordinator's
            // own local adapter verbatim, so the common local-only path is
            // unchanged; remote hosts get an `SshHostAdapter` over a cached
            // ControlMaster. Skipped (default local-only provider retained)
            // only when no engine-owned control-socket dir resolves.
            if let Some(control_dir) = provider_control_dir {
                let local_adapter = execution_coordinator_inner.host_adapter();
                execution_coordinator_inner.set_host_adapter_provider(Arc::new(
                    crate::host_adapter::SshHostAdapterProvider::new(
                        local_adapter,
                        work_db.clone(),
                        cfg_for_provider,
                        provider_events_socket,
                        control_dir,
                    ),
                ));
            }
            let execution_coordinator = Arc::new(execution_coordinator_inner);

            ServerState {
                work_db,
                execution_coordinator,
                completion_handler,
                cube_client: cube_client_for_state,
                publisher: publisher_for_state,
                dispatch_events: dispatch_events_for_state,
                dispatch_event_root: dispatch_event_root_for_state,
                topic_broker,
                worker_registry: WorkerRegistry::new(),
                live_worker_states,
                live_status_manager: Arc::new(LiveStatusManager::new()),
                dispatcher_stats: Arc::new(crate::live_status_loop::DispatcherStats::new(metrics_for_dispatcher)),
                transcript_path_cache: Arc::new(crate::live_status_loop::TranscriptPathCache::new()),
                staged_pr_urls,
                editorial_deny_tracker: Arc::new(crate::editorial_hook::DenyTracker::new()),
                anthropic_api_key,
                worker_pool_size,
                automation_pool_size,
                review_pool_size,
                syspolicyd_health: Arc::new(crate::syspolicyd_monitor::SyspolicydHealth::new()),
                next_session_id: AtomicU64::new(1),
                work_revision,
                app_pid: StdMutex::new(app_pid),
                boss_pid: StdMutex::new(None),
                pending_probes: StdMutex::new(HashMap::new()),
                in_flight_probes: StdMutex::new(HashMap::new()),
                next_probe_id: AtomicU64::new(1),
                app_session: Arc::new(Mutex::new(None)),
                spawn_pane_lock: Arc::new(Mutex::new(())),
                ipc_logger,
                _self_weak: weak_self.clone(),
                feature_flags: feature_flags_for_state,
                capability_registry: Arc::new(crate::feature_flags::CapabilityRegistry::new()),
                settings: settings_for_state,
                metrics: metrics_for_state,
                pr_reconciler_kick: pr_reconciler_kick_for_state,
                automation_scheduler_kick: automation_scheduler_kick_for_state,
                tracker_registry: tracker_registry_for_state,
                github_auth: github_auth_for_state,
                tracker_credential_resolver,
                control_token: control_token_for_state,
                shutdown_trigger: shutdown_trigger_for_state,
            }
        });

        // Register every binary-known counter / gauge handle before
        // any rehydrate or increment runs. `init_all` is empty in
        // phase 1; subsequent phases append one line per new
        // counter module so duplicate-name panics trip during this
        // boot path rather than at runtime (design §"Risks / open
        // questions" item 6).
        crate::metrics::init_all(&server_state.metrics);

        // Seed the in-memory registry from `state.db` so monotonic
        // counter totals span engine restarts. Failures are logged
        // and the registry is left at zero — better than refusing to
        // start because the metrics table is corrupted.
        if let Err(err) = crate::metrics::seed_from_db(&server_state.metrics, &server_state.work_db) {
            tracing::warn!(?err, "metrics: seed_from_db failed; starting from zeroed counters",);
        }

        // Late-bind the runner to the Arc<ServerState>. Going through
        // the WorkerSpawner trait keeps the runner unaware of
        // ServerState's private fields.
        let weak_spawner: Weak<dyn crate::spawn_flow::WorkerSpawner> =
            Arc::downgrade(&server_state) as Weak<dyn crate::spawn_flow::WorkerSpawner>;
        pane_runner.set_server_state(weak_spawner);
        pane_releaser.set_server_state(Arc::downgrade(&server_state));
        probe_queuer.set_server_state(Arc::downgrade(&server_state));

        // Late-bind the scheduler kick into the publisher so the
        // conflict-detection path can wake the scheduler after inserting
        // a ready execution. The coordinator must exist before this is
        // called — hence the late bind.
        let coord_for_kick = server_state.execution_coordinator.clone();
        publisher_impl.set_kick(move || coord_for_kick.kick());

        // Seed the live-status manager's disabled-slot set from the
        // engine metadata KV — survives restarts of the engine
        // process. Empty on first boot.
        let persisted = load_live_status_disabled_slots(&server_state.work_db);
        server_state.live_status_manager.set_initial_disabled_slots(persisted);

        // Seed the dispatch-pause flag from the engine metadata KV.
        // A persisted pause survives an engine restart — the flag is set
        // here before any scheduler kicks so no executions slip through
        // the gap between boot and pause restoration.
        let (dispatch_paused, dispatch_paused_since) = load_dispatch_paused_state(&server_state.work_db);
        if dispatch_paused {
            server_state
                .execution_coordinator
                .set_dispatch_paused(true, dispatch_paused_since);
            tracing::info!(
                paused_since_epoch_s = dispatch_paused_since,
                "dispatch: restoring persisted pause state — dispatch remains \
                 globally paused until `bossctl dispatch resume` is called",
            );
        }

        Ok(server_state)
    }

    /// Send a request to the registered app session and await the
    /// response. Returns `Err` if no app is registered, the app
    /// disconnects before replying, or the request times out.
    pub async fn send_to_app(
        &self,
        request: EngineToAppRequest,
        wait: Duration,
    ) -> Result<EngineToAppResponse, SendToAppError> {
        let (tx, rx) = oneshot::channel();
        let request_id = {
            let mut guard = self.app_session.lock().await;
            let Some(handle) = guard.as_mut() else {
                return Err(SendToAppError::NotRegistered);
            };
            let request_id = handle.allocate_request_id();
            handle.pending.insert(request_id.clone(), tx);
            handle
                .sink
                .enqueue(FrontendEventEnvelope::push(FrontendEvent::EngineRequest {
                    request_id: request_id.clone(),
                    request: request.clone(),
                }));
            request_id
        };

        self.ipc_logger.log_request(&request_id, &request);

        match timeout(wait, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_recv_err)) => {
                self.drop_pending(&request_id).await;
                Err(SendToAppError::AppDisconnected)
            }
            Err(_elapsed) => {
                self.drop_pending(&request_id).await;
                Err(SendToAppError::Timeout)
            }
        }
    }

    async fn drop_pending(&self, request_id: &str) {
        if let Some(handle) = self.app_session.lock().await.as_mut() {
            handle.pending.remove(request_id);
        }
    }

    /// Tear down the libghostty pane allocated for `run_id`.
    /// Idempotent: `take_slot_for_run` returns `None` after the first
    /// call so duplicate releases (completion-detection followed by a
    /// chore-done update or `bossctl agents stop`) don't error out.
    /// Errors talking to the app are logged and swallowed — the slot
    /// mapping has already been removed, so a future release can't
    /// retry without a fresh registration.
    ///
    /// Also drops the matching `LiveWorkerStateRegistry` entry and
    /// broadcasts the snapshot so subscribers (the kanban Doing dot,
    /// the pane titlebar pill) stop showing the worker as attached
    /// to its work item. Without this step a chore-done update would
    /// release the libghostty pane but leave the live state stuck on
    /// `WaitingForInput`, making the UI think the worker was still
    /// running.
    pub async fn release_worker_pane(&self, run_id: &str) -> PaneReleaseOutcome {
        let Some(slot_id) = self.worker_registry.take_slot_for_run(run_id) else {
            tracing::debug!(
                run_id,
                "release_worker_pane: no slot mapped (already released or never spawned)",
            );
            // No mapped slot means no pane and no recorded pid to reap —
            // the worker either already released or has not finished
            // spawning. Either way the caller must not treat this as a
            // reap that frees the workspace lease.
            return PaneReleaseOutcome::NoLiveWorker;
        };
        // Snapshot the worker's recorded shell pid *before* we drop the
        // live-state entry further down — the engine-side reap backstop
        // below needs it. `0` means "pid not reported by the app yet",
        // which the reaper treats as a no-op.
        let shell_pid = self
            .live_worker_states
            .get(slot_id)
            .map(|state| state.shell_pid)
            .unwrap_or(0);
        let request = EngineToAppRequest::ReleaseWorkerPane(ReleaseWorkerPaneInput {
            slot_id,
            kill_grace_seconds: 5,
        });
        match self.send_to_app(request, Duration::from_secs(5)).await {
            Ok(EngineToAppResponse::ReleaseWorkerPane { result: Ok(_) }) => {
                tracing::info!(run_id, slot_id, "released worker pane");
            }
            Ok(EngineToAppResponse::ReleaseWorkerPane {
                result: Err(EngineToAppError::UnknownSlot),
            }) => {
                tracing::debug!(
                    run_id,
                    slot_id,
                    "release_worker_pane: app reports unknown slot — already released",
                );
            }
            Ok(other) => {
                tracing::warn!(
                    run_id,
                    slot_id,
                    ?other,
                    "release_worker_pane: app returned unexpected response",
                );
            }
            Err(SendToAppError::NotRegistered) => {
                tracing::debug!(
                    run_id,
                    slot_id,
                    "release_worker_pane: no app session registered; skipping",
                );
            }
            Err(err) => {
                tracing::warn!(?err, run_id, slot_id, "release_worker_pane: failed");
            }
        }
        // Engine-side reap backstop. The app's pane teardown above is
        // the primary reaper, but it cannot act when no app session is
        // registered, when the app is unresponsive, or when a wedged
        // surface reports no foreground pid — exactly the `bossctl
        // agents stop` leak from #975, where the engine slot and the
        // cube lease were freed but the worker's `claude` process kept
        // running (orphaned, still holding bazel/swiftc locks). Signal
        // the recorded shell pid's process group directly so the OS
        // process tree goes down even when the app path can't reach it.
        // Idempotent with the app's reap: a process already gone just
        // yields `ESRCH`. The grace mirrors the app's `kill_grace_seconds`.
        reap_worker_process_tree(shell_pid, Duration::from_secs(5));
        // The engine's WorkerPool slot was held for the lifetime of
        // the libghostty pane (the coordinator deferred its release
        // when `run_execution` returned with `slot_id = Some(N)`).
        // Now that the pane has been torn down — successfully or
        // not — the engine and the app are back in agreement that
        // slot N is free, so release the pool slot too and kick the
        // scheduler. `WorkerPool::release_worker` is a find-or-skip
        // no-op for already-idle slots, so this is safe even if the
        // pane was a non-pool spawn (e.g. legacy or test path).
        let worker_id = crate::coordinator::worker_id_for_slot(slot_id);
        self.execution_coordinator
            .release_worker_and_kick(&worker_id, None)
            .await;
        // Always drop the live-state entry — we've already given up
        // ownership of the slot in the worker registry, so a stale
        // entry here would lie to the UI about the slot being live.
        self.live_worker_states.release_slot(slot_id);
        // Tear down the per-slot live-status task. The manager
        // doesn't await the task's exit so a wedged Anthropic call
        // can't block the release path.
        self.live_status_manager.stop_slot(slot_id);
        // Drop the cached transcript path for this run so the cache
        // doesn't grow without bound across long engine lifetimes.
        // No correctness consequence — the work_runs row is the
        // durable source of truth — but a bounded cache is hygienic.
        self.transcript_path_cache.forget(run_id);
        self.broadcast_live_worker_states().await;
        // A slot was mapped, so a worker had finished spawning: its pane
        // was torn down and (above) its OS process tree signalled. Report
        // `Reaped` so the caller may free the workspace lease.
        PaneReleaseOutcome::Reaped
    }

    /// Release every live worker pane the engine knows about. Called
    /// from the engine-shutdown path: walks
    /// `LiveWorkerStateRegistry::snapshot()` and dispatches
    /// [`ServerState::release_worker_pane`] for each `run_id` in
    /// parallel. The app teardown is the primary mechanism — once the
    /// pane is released the worker shell exits and `claude` exits
    /// with it.
    ///
    /// `total_timeout` bounds the whole walk. Each individual
    /// `release_worker_pane` call already has its own ~5s round-trip
    /// budget against the app, but on shutdown we'd rather forcibly
    /// move on than block the engine exit on an unresponsive app.
    ///
    /// After the bounded join we send a best-effort `SIGTERM` (then
    /// `SIGKILL` after `kill_grace`) to every recorded `shell_pid > 0`
    /// — covers the case where the app is gone or didn't ack in time
    /// and the shell would otherwise be reparented to launchd.
    pub async fn shutdown_workers(self: &Arc<Self>, total_timeout: Duration, kill_grace: Duration) {
        let snapshot = self.live_worker_states.snapshot();
        if snapshot.is_empty() {
            tracing::info!("shutdown_workers: no live workers to release");
            return;
        }
        tracing::info!(count = snapshot.len(), "shutdown_workers: releasing live worker panes",);
        let mut set = tokio::task::JoinSet::new();
        for state in &snapshot {
            let server = Arc::clone(self);
            let run_id = state.run_id.clone();
            set.spawn(async move {
                server.release_worker_pane(&run_id).await;
            });
        }
        let join_all = async { while set.join_next().await.is_some() {} };
        if tokio::time::timeout(total_timeout, join_all).await.is_err() {
            tracing::warn!(
                timeout_secs = total_timeout.as_secs(),
                "shutdown_workers: release timed out; falling back to direct kill",
            );
        }
        let pids: Vec<libc::pid_t> = snapshot
            .iter()
            .filter_map(|s| (s.shell_pid > 0).then_some(s.shell_pid as libc::pid_t))
            .collect();
        signal_shell_pids(&pids, kill_grace);
    }

    /// Resolve `run_id → slot_id` and ask the app to bring that
    /// worker pane to the front. Returns the resolved slot on success
    /// so callers (`bossctl agents focus`) can confirm in JSON output
    /// which slot was raised.
    pub async fn focus_worker_pane(&self, run_id: &str) -> Result<u8, FocusPaneError> {
        let Some(slot_id) = self.worker_registry.slot_for_run(run_id) else {
            return Err(FocusPaneError::UnknownRun);
        };
        let request = EngineToAppRequest::FocusWorkerPane(FocusWorkerPaneInput { slot_id });
        match self.send_to_app(request, Duration::from_secs(5)).await {
            Ok(EngineToAppResponse::FocusWorkerPane { result: Ok(_) }) => Ok(slot_id),
            Ok(EngineToAppResponse::FocusWorkerPane { result: Err(err) }) => Err(FocusPaneError::App(err)),
            Ok(other) => Err(FocusPaneError::ResponseKindMismatch(format!("{other:?}"))),
            Err(err) => Err(FocusPaneError::Send(err)),
        }
    }

    /// Resolve `run_id → slot_id` and ask the app to write `text`
    /// into that worker pane as if the user had typed it. Returns the
    /// resolved slot on success so `bossctl agents send` can echo back
    /// which pane was targeted (useful when the agent reference was a
    /// crew name). Mirrors [`focus_worker_pane`] in shape; the only
    /// behavioural difference is the engine→app request kind.
    pub async fn send_input_to_worker(&self, run_id: &str, text: String) -> Result<u8, SendInputError> {
        let Some(slot_id) = self.worker_registry.slot_for_run(run_id) else {
            return Err(SendInputError::UnknownRun);
        };
        let request = EngineToAppRequest::SendToPane(SendToPaneInput { slot_id, text });
        match self.send_to_app(request, Duration::from_secs(5)).await {
            Ok(EngineToAppResponse::SendToPane { result: Ok(_) }) => Ok(slot_id),
            Ok(EngineToAppResponse::SendToPane { result: Err(err) }) => Err(SendInputError::App(err)),
            Ok(other) => Err(SendInputError::ResponseKindMismatch(format!("{other:?}"))),
            Err(err) => Err(SendInputError::Send(err)),
        }
    }

    /// Resolve `run_id → slot_id` and ask the app to deliver an Esc
    /// keystroke to that worker pane's pty — equivalent to the human
    /// pressing Esc with the pane focused. The worker run stays
    /// alive; only the in-flight turn is cancelled. Returns the
    /// resolved slot on success so callers (`bossctl agents
    /// interrupt`) can confirm in JSON output which slot received
    /// the interrupt.
    pub async fn interrupt_worker_pane(&self, run_id: &str) -> Result<u8, InterruptPaneError> {
        let Some(slot_id) = self.worker_registry.slot_for_run(run_id) else {
            return Err(InterruptPaneError::UnknownRun);
        };
        let request = EngineToAppRequest::InterruptWorkerPane(InterruptWorkerPaneInput { slot_id });
        match self.send_to_app(request, Duration::from_secs(5)).await {
            Ok(EngineToAppResponse::InterruptWorkerPane { result: Ok(_) }) => Ok(slot_id),
            Ok(EngineToAppResponse::InterruptWorkerPane { result: Err(err) }) => Err(InterruptPaneError::App(err)),
            Ok(other) => Err(InterruptPaneError::ResponseKindMismatch(format!("{other:?}"))),
            Err(err) => Err(InterruptPaneError::Send(err)),
        }
    }

    /// Resolve `id` (short-form `T607` or canonical) to a work item
    /// and ask the app to scroll the kanban to that card and play a
    /// short transient highlight. Returns the canonical id on success
    /// so `bossctl reveal` can confirm what was highlighted.
    pub async fn reveal_work_item(&self, id: &str) -> Result<String, RevealItemError> {
        let item = self
            .work_db
            .get_work_item_resolving_short_id(id)
            .map_err(|_| RevealItemError::NotFound(id.to_owned()))?
            .ok_or_else(|| RevealItemError::NotFound(id.to_owned()))?;
        let canonical_id = match &item {
            crate::work::WorkItem::Task(t) | crate::work::WorkItem::Chore(t) => {
                if t.deleted_at.is_some() {
                    return Err(RevealItemError::Deleted(id.to_owned()));
                }
                t.id.clone()
            }
            crate::work::WorkItem::Project(p) => p.id.clone(),
            crate::work::WorkItem::Product(p) => p.id.clone(),
        };
        let product_id = work_item_product_id(&item);
        let request = EngineToAppRequest::RevealWorkItem(RevealWorkItemInput {
            work_item_id: canonical_id.clone(),
            product_id,
        });
        match self.send_to_app(request, Duration::from_secs(5)).await {
            Ok(EngineToAppResponse::RevealWorkItem { result: Ok(_) }) => Ok(canonical_id),
            Ok(EngineToAppResponse::RevealWorkItem { result: Err(err) }) => Err(RevealItemError::App(err)),
            Ok(other) => Err(RevealItemError::ResponseKindMismatch(format!("{other:?}"))),
            Err(err) => Err(RevealItemError::Send(err)),
        }
    }

    /// Register `session_id` as the app session. Any prior
    /// registration's pending requests are resolved as
    /// `AppDisconnected`.
    async fn register_app_session(&self, session_id: String, sink: Arc<SessionSink>) {
        let prior = self
            .app_session
            .lock()
            .await
            .replace(AppSessionHandle::new(session_id, sink));
        if let Some(prior) = prior {
            for (_, tx) in prior.pending {
                let _ = tx.send(EngineToAppResponse::SpawnWorkerPane {
                    result: Err(EngineToAppError::AppDisconnected),
                });
            }
        }
    }

    /// If `session_id` is the registered app, drop the registration
    /// and resolve all pending requests as `AppDisconnected`.
    async fn drop_app_session_if_matches(&self, session_id: &str) {
        let mut guard = self.app_session.lock().await;
        let take = matches!(guard.as_ref(), Some(handle) if handle.session_id == session_id);
        if take && let Some(prior) = guard.take() {
            for (_, tx) in prior.pending {
                let _ = tx.send(EngineToAppResponse::SpawnWorkerPane {
                    result: Err(EngineToAppError::AppDisconnected),
                });
            }
        }
    }

    /// Snapshot of every allocated worker slot's live runtime state.
    pub fn live_worker_states_snapshot(&self) -> Vec<crate::protocol::LiveWorkerState> {
        self.live_worker_states.snapshot()
    }

    /// Push the current live-worker-state snapshot on the
    /// `worker.live_states` topic. Called whenever the events-socket
    /// consumer or the spawn flow mutates the registry.
    pub async fn broadcast_live_worker_states(&self) {
        let states = self.live_worker_states.snapshot();
        let envelope = FrontendEventEnvelope::push(FrontendEvent::WorkerLiveStatesList { states });
        self.topic_broker.publish(TOPIC_WORKER_LIVE_STATES, envelope).await;
    }

    /// Push the current GitHub OAuth auth state on the `github.auth` topic.
    /// Called by the auth forwarder on every state transition so subscribed
    /// frontends re-render the issue-sync "GitHub account" section as the
    /// device flow advances. The DTO is display-safe — the token and the
    /// private device code never appear in it.
    pub async fn broadcast_github_auth_state(&self, state: GitHubAuthStateDto) {
        let envelope = FrontendEventEnvelope::push(FrontendEvent::GitHubAuthState { state });
        self.topic_broker.publish(TOPIC_GITHUB_AUTH, envelope).await;
    }

    /// Push the current engine-health snapshot on the `engine.health` topic.
    /// Called whenever health-affecting state changes (dispatch pause/resume,
    /// etc.) so subscribed frontends update the health banner without polling
    /// or restarting.
    pub async fn broadcast_engine_health(self: &Arc<Self>) {
        let report = build_engine_health_report(self);
        let envelope = FrontendEventEnvelope::push(FrontendEvent::EngineHealthResult { report });
        self.topic_broker.publish(TOPIC_ENGINE_HEALTH, envelope).await;
    }

    /// Set the Boss session's shell pid (the second trust root). Any
    /// peer whose process tree includes this pid as an ancestor will
    /// satisfy `BossOnly` / `AppOrBoss` checks.
    pub fn set_boss_pid(&self, pid: libc::pid_t) {
        *self.boss_pid.lock().expect("boss_pid mutex poisoned") = Some(pid);
    }

    pub fn current_boss_pid(&self) -> Option<libc::pid_t> {
        *self.boss_pid.lock().expect("boss_pid mutex poisoned")
    }

    /// The pid currently trusted as the macOS app (the `RegisterAppSession`
    /// / RPC-auth trust root). `None` in test mode (no trust root).
    pub fn current_app_pid(&self) -> Option<libc::pid_t> {
        *self.app_pid.lock().expect("app_pid mutex poisoned")
    }

    /// Re-pin the app trust root. Called when a relaunched app
    /// re-registers against a surviving engine with a new pid — the
    /// old pid belongs to a now-dead process, so the live app becomes
    /// the trust root for subsequent engine↔app RPC authorization.
    fn set_app_pid(&self, pid: libc::pid_t) {
        *self.app_pid.lock().expect("app_pid mutex poisoned") = Some(pid);
    }

    /// Push probe text onto the queue for `run_id`, mint a fresh
    /// `probe_id`, and return it so the caller can correlate the
    /// queued probe with the eventual `FrontendEvent::ProbeReplied`
    /// push. Non-urgent probes append to the back (FIFO); urgent
    /// probes push to the front so they fire before any queued
    /// non-urgent probes. The events-socket consumer delivers one
    /// probe per `Stop` event (non-urgent) or per `PostToolUse`
    /// event (urgent).
    pub fn queue_probe(&self, run_id: String, text: String, urgent: bool) -> String {
        let probe_id = self.allocate_probe_id();
        let probe = PendingProbe {
            probe_id: probe_id.clone(),
            text,
            urgent,
        };
        let mut guard = self.pending_probes.lock().expect("pending_probes mutex poisoned");
        let queue = guard.entry(run_id).or_default();
        if urgent {
            queue.push_front(probe);
        } else {
            queue.push_back(probe);
        }
        probe_id
    }

    /// Push a pre-minted `PendingProbe` back onto the front of the
    /// queue for `run_id`. Used when `SendToPane` fails after we've
    /// already popped the probe — the next Stop will retry, and the
    /// caller's `probe_id` stays stable across the retry.
    fn requeue_probe_front(&self, run_id: String, probe: PendingProbe) {
        self.pending_probes
            .lock()
            .expect("pending_probes mutex poisoned")
            .entry(run_id)
            .or_default()
            .push_front(probe);
    }

    fn allocate_probe_id(&self) -> String {
        format!("probe-{}", self.next_probe_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Pop the next pending probe for `run_id`, if any. Called from
    /// the events-socket consumer when a `Stop` event arrives.
    fn pop_pending_probe(&self, run_id: &str) -> Option<PendingProbe> {
        let mut guard = self.pending_probes.lock().expect("pending_probes mutex poisoned");
        let queue = guard.get_mut(run_id)?;
        let probe = queue.pop_front();
        if queue.is_empty() {
            guard.remove(run_id);
        }
        probe
    }

    /// Note that `probe_id` was just dispatched into the worker's
    /// pane for `run_id`. The next `Stop` boundary on this run will
    /// look for an in-flight entry, read the transcript bytes
    /// written after `offset_bytes`, and emit
    /// `FrontendEvent::ProbeReplied`. Any prior in-flight probe for
    /// the same run is overwritten — we only track one outstanding
    /// reply at a time per run, since dispatch is serialized on
    /// `Stop` events.
    fn note_probe_dispatched(
        &self,
        run_id: String,
        probe_id: String,
        transcript_path: Option<String>,
        offset_bytes: u64,
    ) {
        self.in_flight_probes
            .lock()
            .expect("in_flight_probes mutex poisoned")
            .insert(
                run_id,
                InFlightProbe {
                    probe_id,
                    transcript_path,
                    offset_bytes,
                },
            );
    }

    /// Take and return the in-flight probe for `run_id`, if any.
    /// Idempotent on the second pop: a duplicate Stop firing for
    /// the same run gets `None` and the engine emits no second
    /// `ProbeReplied` for the same probe id.
    fn take_in_flight_probe(&self, run_id: &str) -> Option<InFlightProbe> {
        self.in_flight_probes
            .lock()
            .expect("in_flight_probes mutex poisoned")
            .remove(run_id)
    }

    /// Authorize a peer-pid against an RPC tier. Walks up the peer's
    /// process tree (bounded depth) looking for `app_pid` or
    /// `boss_pid` registered as a trust root, with a worker-exclusion
    /// fallback for the `AppOrBoss` and `BossOnly` tiers.
    ///
    /// Returns `true` when `tier == User`, when the trust root is
    /// `None` (test mode), when an ancestor of `peer_pid` matches a
    /// relevant trust root, or — for `AppOrBoss` — when the peer is
    /// not a descendant of any registered worker shell.
    ///
    /// `AppOrBoss` semantics: workers are the only sibling-process
    /// adversary in the V2 threat model, so the gate is "trusted
    /// subtree, OR not a worker descendant". This matters for the
    /// live coordinator: the Boss session may run from a shell that
    /// descends from neither the app nor the registered Boss pid
    /// (e.g. a tmux pane started before the macOS app launched), and
    /// the strict subtree-only check kept rejecting `bossctl agents
    /// transcript`, `bossctl probe`, `bossctl agents stop`, etc. for
    /// the case the work item names. Worker descendants stay rejected
    /// by the fallback's worker-pid exclusion.
    ///
    /// `BossOnly` semantics: the design names the registered Boss
    /// session's shell pid as the canonical trust root. When that pid
    /// is missing (the macOS app hasn't yet sent
    /// `RegisterBossSession`, or runs that don't set up a Boss pane
    /// at all), we fall back to "descendant of the app, not a
    /// descendant of any registered worker shell". Workers each run
    /// in their own libghostty pane whose shell pid is recorded in
    /// `WorkerRegistry`; a `bossctl` invoked from inside a worker
    /// pane therefore descends from a registered worker pid, while
    /// the same call from the Boss pane (or directly under the app
    /// shell) does not. That distinction is enough to keep workers
    /// out of `BossOnly` even with an unregistered Boss pid.
    pub fn authorize_rpc(&self, tier: RpcTier, peer_pid: Option<libc::pid_t>) -> bool {
        if matches!(tier, RpcTier::User) {
            return true;
        }
        let app_pid = self.current_app_pid();
        let boss_pid = self.current_boss_pid();
        if app_pid.is_none() && boss_pid.is_none() {
            // No trust roots are configured at all — treat as
            // permissive (used by in-process tests).
            return true;
        }
        let Some(peer_pid) = peer_pid else {
            return false;
        };
        match tier {
            RpcTier::User => true,
            RpcTier::AppOrBoss => {
                // Fast path: peer descends from a known trust root. Common
                // case is the human running bossctl from the Boss pane
                // (boss_pid descendant), the app shell (app_pid
                // descendant), or a worker pane (also app_pid descendant
                // — workers are siblings under the app).
                let trust_set: Vec<libc::pid_t> = [app_pid, boss_pid].into_iter().flatten().collect();
                if !trust_set.is_empty() && is_descendant_of_any(peer_pid, &trust_set) {
                    return true;
                }
                // Fallback: the coordinator session may run from a shell
                // that descends from neither trust root — e.g. a plain
                // terminal, or a tmux pane started before the macOS app
                // launched, or a separate Claude Code instance steering
                // the engine. The earlier subtree-only gate rejected
                // those legitimate calls. Admit any caller that is *not*
                // a descendant of a registered worker pane shell.
                // Workers are the only sibling-process adversary in the
                // V2 threat model (`docs/designs/main.md` §"Worker
                // isolation"), so excluding worker subtrees is enough to
                // keep `bossctl agents transcript` and friends from
                // leaking one worker's transcript to another worker.
                let worker_pids = self.worker_registry.registered_pids();
                !is_descendant_of_any(peer_pid, &worker_pids)
            }
            RpcTier::BossOnly => {
                if let Some(boss_pid) = boss_pid {
                    return is_descendant_of_any(peer_pid, &[boss_pid]);
                }
                // No Boss pid registered. Trust descendants of the
                // app, but reject anyone descending from a registered
                // worker pane shell — those are workers, not the
                // Boss session.
                let Some(app_pid) = app_pid else {
                    return false;
                };
                if !is_descendant_of_any(peer_pid, &[app_pid]) {
                    return false;
                }
                let worker_pids = self.worker_registry.registered_pids();
                if worker_pids.is_empty() {
                    return true;
                }
                !is_descendant_of_any(peer_pid, &worker_pids)
            }
        }
    }

    /// Route an `EngineResponse` from the app back to the waiting
    /// `send_to_app` caller.
    async fn deliver_app_response(&self, session_id: &str, request_id: &str, response: EngineToAppResponse) {
        self.ipc_logger.log_response(request_id, &response);

        let mut guard = self.app_session.lock().await;
        let Some(handle) = guard.as_mut() else {
            tracing::warn!(request_id, "engine_response dropped: no registered app session",);
            return;
        };
        if handle.session_id != session_id {
            tracing::warn!(request_id, "engine_response dropped: came from non-app session",);
            return;
        }
        match handle.pending.remove(request_id) {
            Some(tx) => {
                let _ = tx.send(response);
            }
            None => {
                tracing::warn!(request_id, "engine_response dropped: no pending request matches",);
            }
        }
    }

    fn allocate_session_id(&self) -> String {
        format!("session-{}", self.next_session_id.fetch_add(1, Ordering::Relaxed))
    }

    fn current_work_revision(&self) -> u64 {
        self.work_revision.load(Ordering::SeqCst)
    }

    fn bump_work_revision(&self) -> u64 {
        self.work_revision.fetch_add(1, Ordering::SeqCst) + 1
    }
}

/// Enable the transient-recovery sweep to nudge a live idle worker via
/// the same `SendToPane` path that `bossctl agents send` uses.
/// `Arc<ServerState>` can then be coerced to `Arc<dyn WorkerNudger>`.
#[async_trait]
impl crate::transient_recovery::WorkerNudger for ServerState {
    async fn nudge_worker(&self, run_id: &str, text: String) -> Result<(), String> {
        self.send_input_to_worker(run_id, text)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

struct BrokerExecutionPublisher {
    topic_broker: Arc<TopicBroker>,
    work_revision: Arc<AtomicU64>,
    /// Late-bound kick function set after the coordinator is created.
    /// `None` until [`BrokerExecutionPublisher::set_kick`] is called;
    /// `kick_scheduler` is a no-op until the coordinator is wired up.
    kick: std::sync::OnceLock<Arc<dyn Fn() + Send + Sync>>,
}

impl BrokerExecutionPublisher {
    fn set_kick(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.kick.set(Arc::new(f));
    }
}

#[async_trait]
impl ExecutionPublisher for BrokerExecutionPublisher {
    async fn publish(&self, execution_id: &str, work_item_id: &str, status: &str, reason: &str) {
        let revision = self.work_revision.fetch_add(1, Ordering::SeqCst) + 1;
        let topic = execution_topic(execution_id);
        let event = FrontendEvent::TopicEvent {
            topic: topic.clone(),
            revision,
            origin_session_id: String::new(),
            origin_request_id: None,
            event: TopicEventPayload::ExecutionInvalidated {
                reason: reason.to_owned(),
                execution_id: execution_id.to_owned(),
                work_item_id: work_item_id.to_owned(),
                status: status.to_owned(),
            },
        };
        self.topic_broker
            .publish(&topic, FrontendEventEnvelope::push_with_revision(revision, event))
            .await;
    }

    async fn publish_work_item_changed(&self, product_id: &str, work_item_id: &str, reason: &str) {
        let revision = self.work_revision.fetch_add(1, Ordering::SeqCst) + 1;
        let topic = work_product_topic(product_id);
        let event = FrontendEvent::TopicEvent {
            topic: topic.clone(),
            revision,
            origin_session_id: String::new(),
            origin_request_id: None,
            event: TopicEventPayload::WorkInvalidated {
                reason: reason.to_owned(),
                product_id: Some(product_id.to_owned()),
                item_ids: vec![work_item_id.to_owned()],
            },
        };
        self.topic_broker
            .publish(&topic, FrontendEventEnvelope::push_with_revision(revision, event))
            .await;
    }

    async fn publish_frontend_event_on_product(&self, product_id: &str, event: FrontendEvent) {
        let revision = self.work_revision.fetch_add(1, Ordering::SeqCst) + 1;
        let topic = work_product_topic(product_id);
        self.topic_broker
            .publish(&topic, FrontendEventEnvelope::push_with_revision(revision, event))
            .await;
    }

    fn kick_scheduler(&self) {
        if let Some(f) = self.kick.get() {
            f();
        }
    }
}

#[async_trait::async_trait]
impl crate::external_tracker::reconcile::WorkInvalidationPublisher for ServerState {
    async fn publish_work_item_invalidated(&self, product_id: &str, work_item_id: &str, reason: &str) {
        self.publisher
            .publish_work_item_changed(product_id, work_item_id, reason)
            .await;
    }
}

/// Maximum events that can be queued for one session before we treat the
/// client as slow. Sized for typical work-invalidation traffic: each
/// mutation emits at most a couple of envelopes, and same-topic
/// invalidations are coalesced, so 256 absorbs bursts while bounding
/// memory.
const MAX_SESSION_QUEUE: usize = 256;

#[derive(Debug, PartialEq, Eq)]
enum EnqueueOutcome {
    Enqueued,
    Coalesced,
    Closed,
    Slow,
}

struct SessionQueue {
    items: VecDeque<FrontendEventEnvelope>,
    /// For each topic with a pending unsent TopicEvent, the index of that
    /// envelope in `items` (front-relative; decremented on pop). Lets us
    /// overwrite stale invalidations instead of growing the queue.
    pending_topics: HashMap<String, usize>,
    closed: bool,
    slow: bool,
}

impl SessionQueue {
    fn new() -> Self {
        Self {
            items: VecDeque::new(),
            pending_topics: HashMap::new(),
            closed: false,
            slow: false,
        }
    }

    fn enqueue(&mut self, env: FrontendEventEnvelope) -> EnqueueOutcome {
        if self.closed {
            return EnqueueOutcome::Closed;
        }
        if self.slow {
            return EnqueueOutcome::Slow;
        }

        if let Some(topic) = topic_event_topic(&env.payload) {
            if let Some(&idx) = self.pending_topics.get(&topic) {
                debug_assert!(idx < self.items.len());
                self.items[idx] = env;
                return EnqueueOutcome::Coalesced;
            }
            if self.items.len() >= MAX_SESSION_QUEUE {
                self.slow = true;
                return EnqueueOutcome::Slow;
            }
            let idx = self.items.len();
            self.items.push_back(env);
            self.pending_topics.insert(topic, idx);
            return EnqueueOutcome::Enqueued;
        }

        if self.items.len() >= MAX_SESSION_QUEUE {
            self.slow = true;
            return EnqueueOutcome::Slow;
        }
        self.items.push_back(env);
        EnqueueOutcome::Enqueued
    }

    fn pop_front(&mut self) -> Option<FrontendEventEnvelope> {
        let env = self.items.pop_front()?;
        // Indices in `pending_topics` are front-relative; shift them down
        // by one and drop the entry that pointed at the just-popped item.
        let mut next = HashMap::with_capacity(self.pending_topics.len());
        for (topic, idx) in self.pending_topics.drain() {
            if idx == 0 {
                continue;
            }
            next.insert(topic, idx - 1);
        }
        self.pending_topics = next;
        Some(env)
    }
}

fn topic_event_topic(payload: &FrontendEvent) -> Option<String> {
    match payload {
        FrontendEvent::TopicEvent { topic, .. } => Some(topic.clone()),
        _ => None,
    }
}

/// Outbound side of one connected session: a bounded coalescing queue plus
/// the shutdown trigger the reader loop selects on. The broker fans
/// invalidations out by calling `enqueue`; the writer task drains via
/// `next`; if either side decides the session is slow or finished, it
/// `close`s the sink and `trigger_shutdown` stops the reader.
struct SessionSink {
    queue: StdMutex<SessionQueue>,
    notify: Notify,
    shutdown: StdMutex<Option<oneshot::Sender<()>>>,
}

impl SessionSink {
    fn new(shutdown_tx: oneshot::Sender<()>) -> Self {
        Self {
            queue: StdMutex::new(SessionQueue::new()),
            notify: Notify::new(),
            shutdown: StdMutex::new(Some(shutdown_tx)),
        }
    }

    fn enqueue(&self, env: FrontendEventEnvelope) -> EnqueueOutcome {
        let outcome = {
            let mut q = self.queue.lock().expect("session queue lock poisoned");
            q.enqueue(env)
        };
        match outcome {
            EnqueueOutcome::Enqueued | EnqueueOutcome::Coalesced => self.notify.notify_one(),
            EnqueueOutcome::Closed | EnqueueOutcome::Slow => {}
        }
        outcome
    }

    fn close(&self) {
        {
            let mut q = self.queue.lock().expect("session queue lock poisoned");
            q.closed = true;
        }
        self.notify.notify_one();
    }

    fn trigger_shutdown(&self) {
        if let Some(tx) = self.shutdown.lock().expect("shutdown lock poisoned").take() {
            let _ = tx.send(());
        }
    }

    /// Wait for the next envelope. Returns `None` once the sink is closed
    /// and the queue is drained.
    async fn next(&self) -> Option<FrontendEventEnvelope> {
        loop {
            // Register interest first so a `notify_one` between our queue
            // peek and the await still wakes us.
            let notified = self.notify.notified();
            let snapshot = {
                let mut q = self.queue.lock().expect("session queue lock poisoned");
                if let Some(env) = q.pop_front() {
                    Some(Some(env))
                } else if q.closed {
                    Some(None)
                } else {
                    None
                }
            };
            match snapshot {
                Some(env_opt) => return env_opt,
                None => notified.await,
            }
        }
    }
}

#[derive(Default)]
struct TopicBroker {
    inner: Mutex<TopicBrokerInner>,
}

#[derive(Default)]
struct TopicBrokerInner {
    sinks: HashMap<String, Arc<SessionSink>>,
    topics_by_session: HashMap<String, HashSet<String>>,
    sessions_by_topic: HashMap<String, HashSet<String>>,
}

impl TopicBroker {
    async fn register_session(&self, session_id: &str, sink: Arc<SessionSink>) {
        let mut inner = self.inner.lock().await;
        inner.sinks.insert(session_id.to_owned(), sink);
    }

    async fn remove_session(&self, session_id: &str) {
        let mut inner = self.inner.lock().await;
        inner.sinks.remove(session_id);
        if let Some(topics) = inner.topics_by_session.remove(session_id) {
            for topic in topics {
                if let Some(sessions) = inner.sessions_by_topic.get_mut(&topic) {
                    sessions.remove(session_id);
                    if sessions.is_empty() {
                        inner.sessions_by_topic.remove(&topic);
                    }
                }
            }
        }
    }

    async fn subscribe(&self, session_id: &str, topics: &[String]) -> Vec<String> {
        let mut inner = self.inner.lock().await;
        let mut added = Vec::new();
        for topic in topics {
            let topic = topic.trim();
            if topic.is_empty() {
                continue;
            }
            let inserted = inner
                .topics_by_session
                .entry(session_id.to_owned())
                .or_default()
                .insert(topic.to_owned());
            inner
                .sessions_by_topic
                .entry(topic.to_owned())
                .or_default()
                .insert(session_id.to_owned());
            if inserted {
                added.push(topic.to_owned());
            }
        }
        added
    }

    async fn unsubscribe(&self, session_id: &str, topics: &[String]) -> Vec<String> {
        let mut inner = self.inner.lock().await;
        let mut removed = Vec::new();
        for topic in topics {
            let topic = topic.trim();
            if topic.is_empty() {
                continue;
            }
            let session_removed = inner
                .topics_by_session
                .get_mut(session_id)
                .map(|session_topics| session_topics.remove(topic))
                .unwrap_or(false);
            if !session_removed {
                continue;
            }
            if let Some(sessions) = inner.sessions_by_topic.get_mut(topic) {
                sessions.remove(session_id);
                if sessions.is_empty() {
                    inner.sessions_by_topic.remove(topic);
                }
            }
            removed.push(topic.to_owned());
        }

        if matches!(
            inner.topics_by_session.get(session_id),
            Some(topics) if topics.is_empty()
        ) {
            inner.topics_by_session.remove(session_id);
        }

        removed
    }

    /// Fan an envelope out to every session subscribed to `topic`. Sessions
    /// whose queue overflows are evicted from the broker and have their
    /// connection torn down — invalidations are cheap to replay by
    /// resubscribing, so a backpressure-stalled client gets disconnected
    /// rather than allowed to balloon engine memory.
    async fn publish(&self, topic: &str, envelope: FrontendEventEnvelope) {
        let sinks = {
            let inner = self.inner.lock().await;
            inner
                .sessions_by_topic
                .get(topic)
                .into_iter()
                .flat_map(|sessions| sessions.iter())
                .filter_map(|session_id| {
                    inner
                        .sinks
                        .get(session_id)
                        .map(|sink| (session_id.clone(), sink.clone()))
                })
                .collect::<Vec<_>>()
        };

        let mut slow = Vec::new();
        for (session_id, sink) in sinks {
            match sink.enqueue(envelope.clone()) {
                EnqueueOutcome::Enqueued | EnqueueOutcome::Coalesced | EnqueueOutcome::Closed => {}
                EnqueueOutcome::Slow => slow.push((session_id, sink)),
            }
        }

        for (session_id, sink) in slow {
            tracing::warn!(
                session_id = %session_id,
                topic,
                "slow subscriber: outbound queue full, disconnecting"
            );
            sink.close();
            sink.trigger_shutdown();
            self.remove_session(&session_id).await;
        }
    }
}

/// Paths derived from a non-default `--socket-path` to ensure a
/// test-fixture engine never touches production state.
///
/// When `socket_path` equals `DEFAULT_SOCKET_PATH` every field is `None` and
/// the engine resolves paths through its normal env-var / home-dir logic.
/// When `socket_path` is non-default, each field is `Some(derived_path)`
/// **unless** the corresponding env override is already set by the caller, in
/// which case the caller's choice wins and that field is `None`.
///
/// The struct is computed once in [`run`] and threaded through to
/// [`run_server`] so both the `WorkConfig` DB path and the socket/pid paths
/// inside [`serve`] use the same derived roots without touching env vars.
struct IsolationPaths {
    /// True when the engine is operating as a test fixture (non-default socket).
    is_test_fixture: bool,
    /// Isolated SQLite DB path derived from the socket stem.
    db_path: Option<std::path::PathBuf>,
    /// Isolated events socket derived from the socket stem.
    events_socket: Option<std::path::PathBuf>,
    /// Isolated pid file derived from the socket stem.
    pid_path: Option<std::path::PathBuf>,
}

impl IsolationPaths {
    /// Derive isolation paths from `socket_path`.
    ///
    /// Non-default socket → derive paths from the socket's directory and
    /// file-stem (e.g. `/tmp/boss-test-UUID.sock` → `/tmp/boss-test-UUID.db`,
    /// `/tmp/boss-test-UUID.events.sock`, `/tmp/boss-test-UUID.pid`).
    ///
    /// Each derived path is suppressed (left as `None`) when the corresponding
    /// env override is already set, so an explicit `BOSS_DB_PATH=…` in the
    /// environment always wins.
    fn derive(socket_path: &str) -> Self {
        if socket_path == DEFAULT_SOCKET_PATH {
            return Self {
                is_test_fixture: false,
                db_path: None,
                events_socket: None,
                pid_path: None,
            };
        }

        let path = std::path::Path::new(socket_path);
        let dir = path.parent().unwrap_or(std::path::Path::new("/tmp"));
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "boss-test".to_owned());

        // Honour explicit env overrides: only set a derived path when the
        // caller hasn't already pointed this socket at an explicit location.
        let db_path = std::env::var_os("BOSS_DB_PATH")
            .is_none()
            .then(|| dir.join(format!("{stem}.db")));
        let events_socket = std::env::var_os("BOSS_EVENTS_SOCKET")
            .is_none()
            .then(|| dir.join(format!("{stem}.events.sock")));
        let pid_path = std::env::var_os("BOSS_ENGINE_PID_PATH")
            .is_none()
            .then(|| dir.join(format!("{stem}.pid")));

        Self {
            is_test_fixture: true,
            db_path,
            events_socket,
            pid_path,
        }
    }
}

async fn handle_frontend_connection(
    stream: UnixStream,
    server_state: Arc<ServerState>,
    peer_pid: Option<libc::pid_t>,
) -> Result<()> {
    tracing::info!("frontend connected");
    let work_db = server_state.work_db.clone();
    let session_id = server_state.allocate_session_id();

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half).lines();

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let sink = Arc::new(SessionSink::new(shutdown_tx));
    server_state
        .topic_broker
        .register_session(&session_id, sink.clone())
        .await;
    let _ = sink.enqueue(FrontendEventEnvelope::push(FrontendEvent::Hello {
        session_id: session_id.clone(),
    }));

    let writer_sink = sink.clone();
    let writer_task = tokio::spawn(async move {
        while let Some(event) = writer_sink.next().await {
            let line = match serde_json::to_string(&event) {
                Ok(line) => line,
                Err(err) => {
                    tracing::error!(?err, "failed to serialize frontend event");
                    continue;
                }
            };

            if let Err(err) = write_half.write_all(line.as_bytes()).await {
                tracing::error!(?err, "failed to write event to frontend socket");
                break;
            }
            if let Err(err) = write_half.write_all(b"\n").await {
                tracing::error!(?err, "failed to delimit frontend event line");
                break;
            }
            if let Err(err) = write_half.flush().await {
                tracing::error!(?err, "failed to flush frontend socket");
                break;
            }
        }
        // Make sure the reader loop wakes if we exited from a write failure
        // rather than an explicit shutdown.
        writer_sink.close();
        writer_sink.trigger_shutdown();
    });

    loop {
        let line_result = tokio::select! {
            _ = &mut shutdown_rx => {
                tracing::info!(session_id = %session_id, "session shutdown triggered");
                break;
            }
            line = reader.next_line() => line,
        };
        let Some(line) = line_result.context("socket read failed")? else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }

        let envelope: FrontendRequestEnvelope = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(err) => {
                send_push(
                    &sink,
                    FrontendEvent::Error {
                        message: format!("invalid request payload: {err}"),
                    },
                );
                continue;
            }
        };
        let request_id = envelope.request_id.clone();
        let request = envelope.payload;

        let ctx = Dispatch::builder()
            .server_state(server_state.clone())
            .work_db(work_db.clone())
            .sink(sink.clone())
            .session_id(session_id.clone())
            .request_id(request_id.clone())
            .maybe_peer_pid(peer_pid)
            .build();
        match request {
            r @ FrontendRequest::AbandonCiRemediation { .. } => {
                ci_remediation::handle_abandon_ci_remediation(ctx, r).await
            }
            r @ FrontendRequest::AbandonConflictResolution { .. } => {
                conflict_resolution::handle_abandon_conflict_resolution(ctx, r).await
            }
            r @ FrontendRequest::ActionAttentionGroup { .. } => attentions::handle_action_attention_group(ctx, r).await,
            r @ FrontendRequest::AddDependency { .. } => dependencies::handle_add_dependency(ctx, r).await,
            r @ FrontendRequest::AddHost { .. } => hosts::handle_add_host(ctx, r).await,
            r @ FrontendRequest::AddHostTag { .. } => hosts::handle_add_host_tag(ctx, r).await,
            r @ FrontendRequest::AnswerAttention { .. } => attentions::handle_answer_attention(ctx, r).await,
            r @ FrontendRequest::AuditProductEffort { .. } => effort::handle_audit_product_effort(ctx, r).await,
            r @ FrontendRequest::CancelExecution { .. } => executions::handle_cancel_execution(ctx, r).await,
            r @ FrontendRequest::ClassifyCiRemediation { .. } => {
                ci_remediation::handle_classify_ci_remediation(ctx, r).await
            }
            r @ FrontendRequest::CommentsApplyMagicWand { .. } => {
                comments::handle_comments_apply_magic_wand(ctx, r).await
            }
            r @ FrontendRequest::CommentsCreate { .. } => comments::handle_comments_create(ctx, r).await,
            r @ FrontendRequest::CommentsDiscardMagicWand { .. } => {
                comments::handle_comments_discard_magic_wand(ctx, r).await
            }
            r @ FrontendRequest::CommentsDismiss { .. } => comments::handle_comments_dismiss(ctx, r).await,
            r @ FrontendRequest::CommentsDispatchMagicWand { .. } => {
                comments::handle_comments_dispatch_magic_wand(ctx, r).await
            }
            r @ FrontendRequest::CommentsList { .. } => comments::handle_comments_list(ctx, r).await,
            r @ FrontendRequest::CommentsResolve { .. } => comments::handle_comments_resolve(ctx, r).await,
            r @ FrontendRequest::CommentsSetStatus { .. } => comments::handle_comments_set_status(ctx, r).await,
            r @ FrontendRequest::CommentsUpdateAnchor { .. } => comments::handle_comments_update_anchor(ctx, r).await,
            r @ FrontendRequest::CreateAttention { .. } => attentions::handle_create_attention(ctx, r).await,
            r @ FrontendRequest::CreateAttentionItem { .. } => attentions::handle_create_attention_item(ctx, r).await,
            r @ FrontendRequest::CreateAutomation { .. } => automations::handle_create_automation(ctx, r).await,
            r @ FrontendRequest::CreateAutomationTask { .. } => {
                automations::handle_create_automation_task(ctx, r).await
            }
            r @ FrontendRequest::CreateChore { .. } => work_items::handle_create_chore(ctx, r).await,
            r @ FrontendRequest::CreateExecution { .. } => executions::handle_create_execution(ctx, r).await,
            r @ FrontendRequest::CreateInvestigation { .. } => work_items::handle_create_investigation(ctx, r).await,
            r @ FrontendRequest::CreateManyChores { .. } => work_items::handle_create_many_chores(ctx, r).await,
            r @ FrontendRequest::CreateManyTasks { .. } => work_items::handle_create_many_tasks(ctx, r).await,
            r @ FrontendRequest::CreateProduct { .. } => products::handle_create_product(ctx, r).await,
            r @ FrontendRequest::CreateProject { .. } => projects::handle_create_project(ctx, r).await,
            r @ FrontendRequest::CreateRevision { .. } => work_items::handle_create_revision(ctx, r).await,
            r @ FrontendRequest::CreateRun { .. } => executions::handle_create_run(ctx, r).await,
            r @ FrontendRequest::CreateTask { .. } => work_items::handle_create_task(ctx, r).await,
            r @ FrontendRequest::DebugLiveStatusPipeline => {
                live_status::handle_debug_live_status_pipeline(ctx, r).await
            }
            r @ FrontendRequest::DeleteAutomation { .. } => automations::handle_delete_automation(ctx, r).await,
            r @ FrontendRequest::DeleteWorkItem { .. } => work_items::handle_delete_work_item(ctx, r).await,
            r @ FrontendRequest::DisableAutomation { .. } => automations::handle_disable_automation(ctx, r).await,
            r @ FrontendRequest::DismissAttention { .. } => attentions::handle_dismiss_attention(ctx, r).await,
            r @ FrontendRequest::EnableAutomation { .. } => automations::handle_enable_automation(ctx, r).await,
            r @ FrontendRequest::EngineResponse { .. } => sessions::handle_engine_response(ctx, r).await,
            r @ FrontendRequest::ExecutionTranscript { .. } => executions::handle_execution_transcript(ctx, r).await,
            r @ FrontendRequest::FindWorkItemsByPr { .. } => work_items::handle_find_work_items_by_pr(ctx, r).await,
            r @ FrontendRequest::FocusWorkerPane { .. } => panes::handle_focus_worker_pane(ctx, r).await,
            r @ FrontendRequest::GetAttentionGroup { .. } => attentions::handle_get_attention_group(ctx, r).await,
            r @ FrontendRequest::GetAttentionItem { .. } => attentions::handle_get_attention_item(ctx, r).await,
            r @ FrontendRequest::GetAutomation { .. } => automations::handle_get_automation(ctx, r).await,
            r @ FrontendRequest::GetAutomationOpenTaskCount { .. } => {
                automations::handle_get_automation_open_task_count(ctx, r).await
            }
            r @ FrontendRequest::GetCiBudget { .. } => ci_remediation::handle_get_ci_budget(ctx, r).await,
            r @ FrontendRequest::GetCiRemediation { .. } => ci_remediation::handle_get_ci_remediation(ctx, r).await,
            r @ FrontendRequest::GetConflictResolution { .. } => {
                conflict_resolution::handle_get_conflict_resolution(ctx, r).await
            }
            r @ FrontendRequest::GetDispatchState => engine_meta::handle_get_dispatch_state(ctx, r).await,
            r @ FrontendRequest::GetEngineHealth => engine_meta::handle_get_engine_health(ctx, r).await,
            r @ FrontendRequest::GetEngineVersion => engine_meta::handle_get_engine_version(ctx, r).await,
            r @ FrontendRequest::GetExecution { .. } => executions::handle_get_execution(ctx, r).await,
            r @ FrontendRequest::GetHost { .. } => hosts::handle_get_host(ctx, r).await,
            r @ FrontendRequest::GetRun { .. } => executions::handle_get_run(ctx, r).await,
            r @ FrontendRequest::GetSettings => engine_meta::handle_get_settings(ctx, r).await,
            r @ FrontendRequest::GetTaskRuntime { .. } => executions::handle_get_task_runtime(ctx, r).await,
            r @ FrontendRequest::GetWorkItem { .. } => work_items::handle_get_work_item(ctx, r).await,
            r @ FrontendRequest::GetWorkItemByShortId { .. } => {
                work_items::handle_get_work_item_by_short_id(ctx, r).await
            }
            r @ FrontendRequest::GetWorkTree { .. } => work_items::handle_get_work_tree(ctx, r).await,
            r @ FrontendRequest::GitHubAuthCancel => github_auth::handle_git_hub_auth_cancel(ctx, r).await,
            r @ FrontendRequest::GitHubAuthDisconnect => github_auth::handle_git_hub_auth_disconnect(ctx, r).await,
            r @ FrontendRequest::GitHubAuthStart => github_auth::handle_git_hub_auth_start(ctx, r).await,
            r @ FrontendRequest::GitHubAuthStatus => github_auth::handle_git_hub_auth_status(ctx, r).await,
            r @ FrontendRequest::InterruptWorkerPane { .. } => panes::handle_interrupt_worker_pane(ctx, r).await,
            r @ FrontendRequest::KickPrReconcilers => engine_meta::handle_kick_pr_reconcilers(ctx, r).await,
            r @ FrontendRequest::LinkWorkItemExternalRef { .. } => {
                external_tracker::handle_link_work_item_external_ref(ctx, r).await
            }
            r @ FrontendRequest::ListAttentionGroups { .. } => attentions::handle_list_attention_groups(ctx, r).await,
            r @ FrontendRequest::ListAttentionItems { .. } => attentions::handle_list_attention_items(ctx, r).await,
            r @ FrontendRequest::ListAttentionItemsForWorkItem { .. } => {
                attentions::handle_list_attention_items_for_work_item(ctx, r).await
            }
            r @ FrontendRequest::ListAutomationRuns { .. } => automations::handle_list_automation_runs(ctx, r).await,
            r @ FrontendRequest::ListAutomations { .. } => automations::handle_list_automations(ctx, r).await,
            r @ FrontendRequest::ListAutomationTasks { .. } => automations::handle_list_automation_tasks(ctx, r).await,
            r @ FrontendRequest::ListChores { .. } => work_items::handle_list_chores(ctx, r).await,
            r @ FrontendRequest::ListCiRemediations { .. } => ci_remediation::handle_list_ci_remediations(ctx, r).await,
            r @ FrontendRequest::ListConflictResolutions { .. } => {
                conflict_resolution::handle_list_conflict_resolutions(ctx, r).await
            }
            r @ FrontendRequest::ListDependencies { .. } => dependencies::handle_list_dependencies(ctx, r).await,
            r @ FrontendRequest::ListDependenciesDetailed { .. } => {
                dependencies::handle_list_dependencies_detailed(ctx, r).await
            }
            r @ FrontendRequest::ListEditorialActions { .. } => {
                automations::handle_list_editorial_actions(ctx, r).await
            }
            r @ FrontendRequest::ListEngineAttempts { .. } => executions::handle_list_engine_attempts(ctx, r).await,
            r @ FrontendRequest::ListExecutions { .. } => executions::handle_list_executions(ctx, r).await,
            r @ FrontendRequest::ListFeatureFlags => engine_meta::handle_list_feature_flags(ctx, r).await,
            r @ FrontendRequest::ListHosts => hosts::handle_list_hosts(ctx, r).await,
            r @ FrontendRequest::ListLiveStatusDisabledSlots => {
                live_status::handle_list_live_status_disabled_slots(ctx, r).await
            }
            r @ FrontendRequest::ListProducts => products::handle_list_products(ctx, r).await,
            r @ FrontendRequest::ListProjects { .. } => projects::handle_list_projects(ctx, r).await,
            r @ FrontendRequest::ListRuns { .. } => executions::handle_list_runs(ctx, r).await,
            r @ FrontendRequest::ListTasks { .. } => work_items::handle_list_tasks(ctx, r).await,
            r @ FrontendRequest::ListRevisions { .. } => work_items::handle_list_revisions(ctx, r).await,
            r @ FrontendRequest::ListWorkerLiveStates => panes::handle_list_worker_live_states(ctx, r).await,
            r @ FrontendRequest::MarkCiRemediationFailed { .. } => {
                ci_remediation::handle_mark_ci_remediation_failed(ctx, r).await
            }
            r @ FrontendRequest::MarkCiRemediationNoop { .. } => {
                ci_remediation::handle_mark_ci_remediation_noop(ctx, r).await
            }
            r @ FrontendRequest::MarkCiRemediationRetriggered { .. } => {
                ci_remediation::handle_mark_ci_remediation_retriggered(ctx, r).await
            }
            r @ FrontendRequest::MarkCiRemediationSucceededViaRebase { .. } => {
                ci_remediation::handle_mark_ci_remediation_succeeded_via_rebase(ctx, r).await
            }
            r @ FrontendRequest::MarkConflictResolutionFailed { .. } => {
                conflict_resolution::handle_mark_conflict_resolution_failed(ctx, r).await
            }
            r @ FrontendRequest::MergeWhenReady { .. } => review::handle_merge_when_ready(ctx, r).await,
            r @ FrontendRequest::MetricsListLive => metrics::handle_metrics_list_live(ctx, r).await,
            r @ FrontendRequest::MetricsReset { .. } => metrics::handle_metrics_reset(ctx, r).await,
            r @ FrontendRequest::MetricsShowLive { .. } => metrics::handle_metrics_show_live(ctx, r).await,
            r @ FrontendRequest::OpenReviewTerminal { .. } => review::handle_open_review_terminal(ctx, r).await,
            r @ FrontendRequest::ProbeRun { .. } => executions::handle_probe_run(ctx, r).await,
            r @ FrontendRequest::ReapRun { .. } => executions::handle_reap_run(ctx, r).await,
            r @ FrontendRequest::RecordEffortEscalation { .. } => effort::handle_record_effort_escalation(ctx, r).await,
            r @ FrontendRequest::RegisterAppSession => sessions::handle_register_app_session(ctx, r).await,
            r @ FrontendRequest::RegisterBossSession { .. } => sessions::handle_register_boss_session(ctx, r).await,
            r @ FrontendRequest::RegisterCapabilities { .. } => engine_meta::handle_register_capabilities(ctx, r).await,
            r @ FrontendRequest::ReleaseReviewTerminal { .. } => review::handle_release_review_terminal(ctx, r).await,
            r @ FrontendRequest::RemoveDependency { .. } => dependencies::handle_remove_dependency(ctx, r).await,
            r @ FrontendRequest::RemoveHost { .. } => hosts::handle_remove_host(ctx, r).await,
            r @ FrontendRequest::RemoveHostTag { .. } => hosts::handle_remove_host_tag(ctx, r).await,
            r @ FrontendRequest::ReorderProjectTasks { .. } => projects::handle_reorder_project_tasks(ctx, r).await,
            r @ FrontendRequest::RequestExecution { .. } => executions::handle_request_execution(ctx, r).await,
            r @ FrontendRequest::ResolveProjectDesignDoc { .. } => {
                projects::handle_resolve_project_design_doc(ctx, r).await
            }
            r @ FrontendRequest::RestoreWorkItem { .. } => work_items::handle_restore_work_item(ctx, r).await,
            r @ FrontendRequest::RetryCiRemediation { .. } => ci_remediation::handle_retry_ci_remediation(ctx, r).await,
            r @ FrontendRequest::RetryConflictResolution { .. } => {
                conflict_resolution::handle_retry_conflict_resolution(ctx, r).await
            }
            r @ FrontendRequest::RevealWorkItem { .. } => work_items::handle_reveal_work_item(ctx, r).await,
            r @ FrontendRequest::RunAutomation { .. } => automations::handle_run_automation(ctx, r).await,
            r @ FrontendRequest::SendInputToWorker { .. } => panes::handle_send_input_to_worker(ctx, r).await,
            r @ FrontendRequest::SetCiBudget { .. } => ci_remediation::handle_set_ci_budget(ctx, r).await,
            r @ FrontendRequest::SetDispatchPaused { .. } => engine_meta::handle_set_dispatch_paused(ctx, r).await,
            r @ FrontendRequest::SetFeatureFlag { .. } => engine_meta::handle_set_feature_flag(ctx, r).await,
            r @ FrontendRequest::SetHostEnabled { .. } => hosts::handle_set_host_enabled(ctx, r).await,
            r @ FrontendRequest::SetLiveStatusEnabled { .. } => {
                live_status::handle_set_live_status_enabled(ctx, r).await
            }
            r @ FrontendRequest::SetProductDefaultModel { .. } => {
                products::handle_set_product_default_model(ctx, r).await
            }
            r @ FrontendRequest::SetProductDefaultDriver { .. } => {
                products::handle_set_product_default_driver(ctx, r).await
            }
            r @ FrontendRequest::SetProductEditorialRules { .. } => {
                products::handle_set_product_editorial_rules(ctx, r).await
            }
            r @ FrontendRequest::EvaluateEditorialRules { .. } => {
                products::handle_evaluate_editorial_rules(ctx, r).await
            }
            r @ FrontendRequest::SetProductExternalTracker { .. } => {
                external_tracker::handle_set_product_external_tracker(ctx, r).await
            }
            r @ FrontendRequest::SetProjectDesignDoc { .. } => projects::handle_set_project_design_doc(ctx, r).await,
            r @ FrontendRequest::SetSetting { .. } => engine_meta::handle_set_setting(ctx, r).await,
            r @ FrontendRequest::Shutdown { .. } => sessions::handle_shutdown(ctx, r).await,
            r @ FrontendRequest::StopRun { .. } => executions::handle_stop_run(ctx, r).await,
            r @ FrontendRequest::Subscribe { .. } => subscriptions::handle_subscribe(ctx, r).await,
            r @ FrontendRequest::SyncProductExternalTracker { .. } => {
                external_tracker::handle_sync_product_external_tracker(ctx, r).await
            }
            r @ FrontendRequest::TailRunTranscript { .. } => executions::handle_tail_run_transcript(ctx, r).await,
            r @ FrontendRequest::UnlinkWorkItemExternalRef { .. } => {
                external_tracker::handle_unlink_work_item_external_ref(ctx, r).await
            }
            r @ FrontendRequest::Unsubscribe { .. } => subscriptions::handle_unsubscribe(ctx, r).await,
            r @ FrontendRequest::UpdateAutomation { .. } => automations::handle_update_automation(ctx, r).await,
            r @ FrontendRequest::UpdateWorkItem { .. } => work_items::handle_update_work_item(ctx, r).await,
            r @ FrontendRequest::UpdateWorkerShellPid { .. } => sessions::handle_update_worker_shell_pid(ctx, r).await,
            r @ FrontendRequest::WorkspacePoolSummary => engine_meta::handle_workspace_pool_summary(ctx, r).await,
        }
    }

    server_state.topic_broker.remove_session(&session_id).await;
    server_state.drop_app_session_if_matches(&session_id).await;
    sink.close();
    let _ = writer_task.await;
    Ok(())
}
