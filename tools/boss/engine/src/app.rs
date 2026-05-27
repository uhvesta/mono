use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify, oneshot};

use crate::audit_effort;
use crate::cli::Cli;
use crate::ipc_log::IpcLogger;
use crate::completion::{
    CommandPrDetector, PrDetector, ProbeQueuer, WorkerCompletionHandler, WorkerPaneReleaser,
};
use crate::config::RuntimeConfig;
use crate::coordinator::{
    CommandCubeClient, CubeClient, ExecutionCoordinator, ExecutionPublisher, WorkerPool,
};
use crate::events_socket::{bind_events_socket, handle_connection, peer_pid};
use crate::live_status_loop::{
    LiveStatusBroadcaster, LiveStatusManager, TranscriptPathResolver, Trigger,
};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::merge_poller::{CommandMergeProbe, MergeProbe, spawn_loop as spawn_merge_poller};
use crate::protocol::{
    EngineToAppError, EngineToAppRequest, EngineToAppResponse, FocusWorkerPaneInput, FrontendEvent,
    FrontendEventEnvelope, FrontendRequest, FrontendRequestEnvelope, InterruptWorkerPaneInput,
    ReleaseWorkerPaneInput, RequestExecutionInput, RevealWorkItemInput, SendToPaneInput,
    TOPIC_WORK_PRODUCTS, TOPIC_WORKER_LIVE_STATES, TopicEventPayload, execution_topic, probe_topic,
    work_product_topic,
};
use crate::work::{DuplicateTaskError, GhPrStateChecker, SetRunTranscriptPathOutcome, Task, WorkDb, WorkItem};
use crate::worker_registry::WorkerRegistry;
use async_trait::async_trait;
use tokio::time::{Duration, timeout};

const DEFAULT_SOCKET_PATH: &str = "/tmp/boss-engine.sock";
const DEFAULT_PID_PATH: &str = "/tmp/boss-engine.pid";

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

    fn live_worker_state_registry(&self) -> Option<&LiveWorkerStateRegistry> {
        Some(&self.live_worker_states)
    }

    async fn publish_live_worker_states(&self) {
        self.broadcast_live_worker_states().await;
    }

    fn start_live_status_slot(&self, slot_id: u8, run_id: &str) {
        let Some(arc_self) = self._self_weak.upgrade() else {
            tracing::debug!(
                slot_id,
                "start_live_status_slot: ServerState already dropped",
            );
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
    async fn release_pane(&self, run_id: &str) {
        let Some(weak) = self.server.get() else {
            tracing::warn!(run_id, "pane releaser called before server state was bound");
            return;
        };
        let Some(server) = weak.upgrade() else {
            tracing::debug!(run_id, "pane releaser: server state already dropped");
            return;
        };
        server.release_worker_pane(run_id).await;
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
    /// Primary-path resolution-signal staging for `conflict_resolution`
    /// executions. Populated by [`dispatch_live_worker_state`] from
    /// `PostToolUse` Bash hooks that are force-push commands or
    /// PR-comment posts. Read by [`WorkerCompletionHandler::on_stop`]
    /// to transition the parent chore `blocked → in_review` immediately
    /// on Stop, without waiting for the merge-poller sweep.
    ///
    /// Shared with the completion handler via
    /// [`WorkerCompletionHandler::with_staged_resolution_signals`].
    staged_resolution_signals:
        Arc<crate::resolution_signal_capture::StagedResolutionSignalCache>,
    /// Snapshot of the Anthropic API key captured at engine startup.
    /// Used by the live-status summarizer for the per-slot task; the
    /// pane-titlebar summarizer continues to resolve the key
    /// per-spawn via `cfg.agent()`.
    anthropic_api_key: Option<String>,
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
    /// Shared kick signal for the merge-poller loop. The macOS app
    /// fires [`FrontendRequest::KickPrReconcilers`] on window
    /// activation; the handler calls `notify_one()` here so the
    /// poller's next wait arm resolves immediately (subject to the
    /// 15 s engine-side quiesce window). `None` only between
    /// `new_arc` return and the first `spawn_merge_poller` call in
    /// `serve` — that window is < 1 ms in production.
    pr_reconciler_kick: Arc<Notify>,
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
        let anthropic_api_key = cfg
            .agent()
            .ok()
            .and_then(|agent| agent.anthropic_api_key.clone());
        // One-time startup signal so the missing-API-key case is
        // immediately visible in engine stderr — the chore calls out
        // that the summarizer used to drop this silently and the user
        // wants to confirm it's not the failure mode they're hitting.
        // Logged at `info` for the happy path so a `grep "live_status:"`
        // sweep still shows the engine made a decision.
        if anthropic_api_key.is_some() {
            tracing::info!(
                "live_status: ANTHROPIC_API_KEY is configured; summarizer enabled",
            );
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
        let staged_resolution_signals = Arc::new(
            crate::resolution_signal_capture::StagedResolutionSignalCache::new(),
        );

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
        let shutdown_trigger = Arc::new(Notify::new());
        let shutdown_trigger_for_state = shutdown_trigger.clone();
        let control_token_for_state = control_token.clone();

        let mut tracker_registry = crate::external_tracker::TrackerRegistry::new();
        tracker_registry
            .register(Arc::new(
                crate::external_tracker::github::GitHubTracker::new(),
            ))
            .expect("github tracker is the only registered kind; duplicate is impossible");
        let tracker_registry = Arc::new(tracker_registry);
        let tracker_registry_for_state = tracker_registry.clone();

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
            .with_staged_resolution_signals(staged_resolution_signals.clone())
            .with_feature_flags(feature_flags_for_handler)
            .with_merge_probe(ci_probe)
            .with_metrics(metrics_for_completion),
        );

        // Build PaneSpawnRunner up front, hand its Weak<ServerState>
        // pointer back via set_server_state once the Arc exists. The
        // runner needs to call into ServerState (send_to_app +
        // worker_registry) while ServerState owns the runner —
        // Arc::new_cyclic breaks the cycle.
        let pane_runner = Arc::new(crate::runner::PaneSpawnRunner::new(
            cfg.clone(),
            work_db.clone(),
        ));
        let runner_for_coordinator = pane_runner.clone();
        let cube_client_for_state = cube_client.clone();
        let publisher_for_state = publisher.clone();

        // Dispatch-event JSONL stream lands next to state.db /
        // events.sock under the same `state_root` resolved above.
        let dispatch_event_root: PathBuf = state_root.clone();
        let dispatch_events: Arc<dyn crate::dispatch_events::DispatchEventSink> = Arc::new(
            crate::dispatch_events::JsonlFileSink::new(dispatch_event_root.clone()),
        );
        let dispatch_events_for_state = dispatch_events.clone();
        let dispatch_event_root_for_state = dispatch_event_root.clone();
        let ipc_logger = IpcLogger::new(&dispatch_event_root);

        let completion_handler_for_coordinator = completion_handler.clone();
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
            // Wire the SHA-delta gate's run-start snapshot: when an
            // execution transitions to `running`, the completion
            // handler captures the bound chore PR's head SHA into
            // `work_executions.pr_head_before`.
            execution_coordinator_inner.set_execution_started_hook(
                completion_handler_for_coordinator.clone(),
            );
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
                live_worker_states: Arc::new(LiveWorkerStateRegistry::new()),
                live_status_manager: Arc::new(LiveStatusManager::new()),
                dispatcher_stats: Arc::new(crate::live_status_loop::DispatcherStats::new(
                    metrics_for_dispatcher,
                )),
                transcript_path_cache: Arc::new(
                    crate::live_status_loop::TranscriptPathCache::new(),
                ),
                staged_pr_urls,
                staged_resolution_signals,
                anthropic_api_key,
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
                settings: settings_for_state,
                metrics: metrics_for_state,
                pr_reconciler_kick: pr_reconciler_kick_for_state,
                tracker_registry: tracker_registry_for_state,
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
        if let Err(err) =
            crate::metrics::seed_from_db(&server_state.metrics, &server_state.work_db)
        {
            tracing::warn!(
                ?err,
                "metrics: seed_from_db failed; starting from zeroed counters",
            );
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
        server_state
            .live_status_manager
            .set_initial_disabled_slots(persisted);

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
    pub async fn release_worker_pane(&self, run_id: &str) {
        let Some(slot_id) = self.worker_registry.take_slot_for_run(run_id) else {
            tracing::debug!(
                run_id,
                "release_worker_pane: no slot mapped (already released or never spawned)",
            );
            return;
        };
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
        // The engine's WorkerPool slot was held for the lifetime of
        // the libghostty pane (the coordinator deferred its release
        // when `run_execution` returned with `slot_id = Some(N)`).
        // Now that the pane has been torn down — successfully or
        // not — the engine and the app are back in agreement that
        // slot N is free, so release the pool slot too and kick the
        // scheduler. `WorkerPool::release_worker` is a find-or-skip
        // no-op for already-idle slots, so this is safe even if the
        // pane was a non-pool spawn (e.g. legacy or test path).
        let worker_id = WorkerPool::worker_id_for_slot(slot_id);
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
        tracing::info!(
            count = snapshot.len(),
            "shutdown_workers: releasing live worker panes",
        );
        let mut set = tokio::task::JoinSet::new();
        for state in &snapshot {
            let server = Arc::clone(self);
            let run_id = state.run_id.clone();
            set.spawn(async move {
                server.release_worker_pane(&run_id).await;
            });
        }
        let join_all = async {
            while set.join_next().await.is_some() {}
        };
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
            Ok(EngineToAppResponse::FocusWorkerPane { result: Err(err) }) => {
                Err(FocusPaneError::App(err))
            }
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
    pub async fn send_input_to_worker(
        &self,
        run_id: &str,
        text: String,
    ) -> Result<u8, SendInputError> {
        let Some(slot_id) = self.worker_registry.slot_for_run(run_id) else {
            return Err(SendInputError::UnknownRun);
        };
        let request = EngineToAppRequest::SendToPane(SendToPaneInput { slot_id, text });
        match self.send_to_app(request, Duration::from_secs(5)).await {
            Ok(EngineToAppResponse::SendToPane { result: Ok(_) }) => Ok(slot_id),
            Ok(EngineToAppResponse::SendToPane { result: Err(err) }) => {
                Err(SendInputError::App(err))
            }
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
        let request =
            EngineToAppRequest::InterruptWorkerPane(InterruptWorkerPaneInput { slot_id });
        match self.send_to_app(request, Duration::from_secs(5)).await {
            Ok(EngineToAppResponse::InterruptWorkerPane { result: Ok(_) }) => Ok(slot_id),
            Ok(EngineToAppResponse::InterruptWorkerPane { result: Err(err) }) => {
                Err(InterruptPaneError::App(err))
            }
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
            Ok(EngineToAppResponse::RevealWorkItem { result: Err(err) }) => {
                Err(RevealItemError::App(err))
            }
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
        if take {
            if let Some(prior) = guard.take() {
                for (_, tx) in prior.pending {
                    let _ = tx.send(EngineToAppResponse::SpawnWorkerPane {
                        result: Err(EngineToAppError::AppDisconnected),
                    });
                }
            }
        }
    }

    pub fn worker_registry_handle(&self) -> &WorkerRegistry {
        &self.worker_registry
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
        self.topic_broker
            .publish(TOPIC_WORKER_LIVE_STATES, envelope)
            .await;
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
        let mut guard = self
            .pending_probes
            .lock()
            .expect("pending_probes mutex poisoned");
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
        format!(
            "probe-{}",
            self.next_probe_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Pop the next pending probe for `run_id`, if any. Called from
    /// the events-socket consumer when a `Stop` event arrives.
    fn pop_pending_probe(&self, run_id: &str) -> Option<PendingProbe> {
        let mut guard = self
            .pending_probes
            .lock()
            .expect("pending_probes mutex poisoned");
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
                let trust_set: Vec<libc::pid_t> =
                    [app_pid, boss_pid].into_iter().flatten().collect();
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
    async fn deliver_app_response(
        &self,
        session_id: &str,
        request_id: &str,
        response: EngineToAppResponse,
    ) {
        self.ipc_logger.log_response(request_id, &response);

        let mut guard = self.app_session.lock().await;
        let Some(handle) = guard.as_mut() else {
            tracing::warn!(
                request_id,
                "engine_response dropped: no registered app session",
            );
            return;
        };
        if handle.session_id != session_id {
            tracing::warn!(
                request_id,
                "engine_response dropped: came from non-app session",
            );
            return;
        }
        match handle.pending.remove(request_id) {
            Some(tx) => {
                let _ = tx.send(response);
            }
            None => {
                tracing::warn!(
                    request_id,
                    "engine_response dropped: no pending request matches",
                );
            }
        }
    }

    fn allocate_session_id(&self) -> String {
        format!(
            "session-{}",
            self.next_session_id.fetch_add(1, Ordering::Relaxed)
        )
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
            .publish(
                &topic,
                FrontendEventEnvelope::push_with_revision(revision, event),
            )
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
            .publish(
                &topic,
                FrontendEventEnvelope::push_with_revision(revision, event),
            )
            .await;
    }

    async fn publish_frontend_event_on_product(
        &self,
        product_id: &str,
        event: FrontendEvent,
    ) {
        let revision = self.work_revision.fetch_add(1, Ordering::SeqCst) + 1;
        let topic = work_product_topic(product_id);
        self.topic_broker
            .publish(
                &topic,
                FrontendEventEnvelope::push_with_revision(revision, event),
            )
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
    async fn publish_work_item_invalidated(
        &self,
        product_id: &str,
        work_item_id: &str,
        reason: &str,
    ) {
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

pub async fn run(cli: Cli) -> Result<()> {
    let socket_str = cli.socket_path.as_deref().unwrap_or(DEFAULT_SOCKET_PATH);
    let isolation = IsolationPaths::derive(socket_str);

    // Build WorkConfig, overriding db_path when the isolation guard derived one.
    // This must happen before RuntimeConfig so the DB the engine opens is
    // already the isolated one — not the production state.db that
    // WorkConfig::load_from_env() would resolve from $HOME.
    let mut work = crate::config::WorkConfig::load_from_env()?;
    if let Some(ref iso_db) = isolation.db_path {
        work.db_path = iso_db.clone();
    }
    let cfg = Arc::new(crate::config::RuntimeConfig::from_parts(work, None));

    if isolation.is_test_fixture {
        tracing::info!(
            cwd = %cfg.work.cwd.display(),
            db_path = %cfg.work.db_path.display(),
            events_socket = ?isolation.events_socket,
            pid_path = ?isolation.pid_path,
            "test-fixture mode: isolated paths derived from non-default socket; \
             production state (events.sock, state.db, pid file) will not be touched"
        );
    } else {
        tracing::info!(
            cwd = %cfg.work.cwd.display(),
            db_path = %cfg.work.db_path.display(),
            "starting boss-engine runtime",
        );
    }

    run_server(cli, cfg, isolation).await
}

async fn run_server(cli: Cli, cfg: Arc<RuntimeConfig>, isolation: IsolationPaths) -> Result<()> {
    let socket_path = cli
        .socket_path
        .unwrap_or_else(|| DEFAULT_SOCKET_PATH.to_owned());

    // Use the isolation-derived pid path, falling back to env / hard default.
    let pid_file_path = isolation
        .pid_path
        .or_else(|| {
            std::env::var("BOSS_ENGINE_PID_PATH")
                .ok()
                .map(std::path::PathBuf::from)
        })
        .unwrap_or_else(|| DEFAULT_PID_PATH.into());

    // Use the isolation-derived events socket, falling back to env / home default.
    let events_socket_path = isolation
        .events_socket
        .map(Ok)
        .unwrap_or_else(default_events_socket_path)?;

    let control_token_path = crate::engine_control::default_token_path();

    // Orphan watcher: when the engine is a test fixture (non-default socket),
    // watch the parent process pid.  If the parent exits (e.g. a `bazel test`
    // runner that failed mid-run), this engine should exit too rather than
    // becoming an orphan that keeps production state bound.
    let watched_parent_pid = if isolation.is_test_fixture {
        let ppid = unsafe { libc::getppid() };
        tracing::debug!(parent_pid = ppid, "orphan watcher armed");
        Some(ppid)
    } else {
        None
    };

    serve(
        cfg,
        socket_path.into(),
        Some(pid_file_path),
        Some(events_socket_path),
        control_token_path,
        watched_parent_pid,
    )
    .await
}

fn default_events_socket_path() -> Result<std::path::PathBuf> {
    if let Ok(override_path) = std::env::var("BOSS_EVENTS_SOCKET") {
        return Ok(override_path.into());
    }
    let Some(home) = std::env::var_os("HOME") else {
        bail!("HOME must be set to derive the default events socket path");
    };
    Ok(std::path::PathBuf::from(home).join("Library/Application Support/Boss/events.sock"))
}

/// Return `true` if the process at `pid` is still alive on this machine.
///
/// Uses `kill(pid, 0)` (signal 0 = probe, no signal delivered): returns `true`
/// when the kernel confirms the process exists.  `EPERM` (process exists but
/// we can't signal it) also counts as alive; only `ESRCH` (no such process)
/// means dead.
pub fn process_is_alive(pid: libc::pid_t) -> bool {
    if pid <= 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    errno == libc::EPERM
}

/// Run the frontend server until the listener fails.
///
/// `socket_path` is bound exclusively (the file is removed first if it exists).
/// When `pid_file_path` is `Some`, the engine writes its pid there and removes
/// the file on shutdown — pass `None` from in-process tests to avoid touching
/// shared filesystem state. When `events_socket_path` is `Some`, the engine
/// also binds the worker events socket (mode 0600) and runs an accept loop
/// that decodes hook payloads via the worker registry; pass `None` from
/// tests that don't exercise the events channel.
///
/// When `control_token_path` is `Some`, the engine mints a random
/// secret on startup, writes it to that path (mode 0600), and accepts
/// matching `Shutdown { token }` RPCs on the frontend socket. The
/// file is removed on graceful exit via [`crate::engine_control::ControlTokenGuard`].
/// Tests pass `None` to skip the file entirely; in-process callers
/// own the runtime handle and don't need an authenticated wire path.
///
/// When `watched_parent_pid` is `Some(ppid)`, a background task polls
/// `kill(ppid, 0)` once per second; if the process is gone the task fires an
/// orphan-shutdown trigger that causes this function to return `Ok(())`.
/// Pass `None` from in-process tests that don't need orphan detection.
pub async fn serve(
    cfg: Arc<RuntimeConfig>,
    socket_path: std::path::PathBuf,
    pid_file_path: Option<std::path::PathBuf>,
    events_socket_path: Option<std::path::PathBuf>,
    control_token_path: Option<std::path::PathBuf>,
    watched_parent_pid: Option<libc::pid_t>,
) -> Result<()> {
    let app_pid = current_parent_pid();
    let (control_token, _control_token_guard) = match control_token_path {
        Some(path) => {
            let token = crate::engine_control::generate_token();
            let contents = crate::engine_control::ControlTokenFile {
                token: token.clone(),
                socket_path: socket_path.display().to_string(),
                pid: std::process::id(),
            };
            crate::engine_control::write_token_file(&path, &contents).with_context(|| {
                format!(
                    "failed to write engine-control token file {}",
                    path.display()
                )
            })?;
            tracing::info!(
                token_path = %path.display(),
                "engine-control token: ready",
            );
            let guard = crate::engine_control::ControlTokenGuard::new(
                path.clone(),
                std::process::id(),
            );
            (Some(Arc::new(token)), Some(guard))
        }
        None => (None, None),
    };
    let server_state =
        ServerState::new_arc_with_app_pid(cfg.clone(), app_pid, control_token.clone())?;

    // Always attempt to unlink any existing file at the path before
    // binding. `path.exists()` lies for dangling symlinks and races
    // with concurrent file ops; just call `remove_file` and ignore
    // `NotFound`. A stale file from a previous engine that crashed
    // without cleanup is the exact failure shape the 2026-05-07
    // incident left behind on `events.sock`; mirror the defensive
    // unlink here so the frontend socket can't develop the same drift.
    match tokio::fs::remove_file(&socket_path).await {
        Ok(()) => {
            tracing::info!(
                socket_path = %socket_path.display(),
                "frontend socket: unlinked stale file before bind",
            );
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(anyhow::Error::new(err)
                .context(format!("failed to remove existing socket {}", socket_path.display())));
        }
    }

    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => {
            crate::audit::record_socket_bind(
                "frontend",
                &socket_path,
                crate::audit::SocketBindResult::Succeeded,
            );
            listener
        }
        Err(err) => {
            let msg = err.to_string();
            crate::audit::record_socket_bind(
                "frontend",
                &socket_path,
                crate::audit::SocketBindResult::Failed(&msg),
            );
            return Err(anyhow::Error::new(err)
                .context(format!("failed to bind unix socket {}", socket_path.display())));
        }
    };

    let _pid_guard = match pid_file_path {
        Some(path) => {
            let path_str = path.to_string_lossy().into_owned();
            let pid = std::process::id();
            std::fs::write(&path, format!("{pid}\n"))
                .with_context(|| format!("failed to write pid file {path_str}"))?;
            tracing::info!(pid, pid_file = %path_str, "engine pid file is ready");
            Some(PidFileGuard {
                path: path_str,
                pid,
            })
        }
        None => None,
    };

    tracing::info!(socket_path = %socket_path.display(), "frontend socket is ready");
    println!("boss-engine listening on {}", socket_path.display());

    if let Some(path) = events_socket_path {
        let events_listener = match bind_events_socket(&path) {
            Ok(listener) => {
                crate::audit::record_socket_bind(
                    "events",
                    &path,
                    crate::audit::SocketBindResult::Succeeded,
                );
                listener
            }
            Err(err) => {
                let msg = err.to_string();
                crate::audit::record_socket_bind(
                    "events",
                    &path,
                    crate::audit::SocketBindResult::Failed(&msg),
                );
                return Err(anyhow::Error::new(err)
                    .context(format!("failed to bind events socket {}", path.display())));
            }
        };
        tracing::info!(events_socket_path = %path.display(), "events socket is ready");
        let server_state_for_events = server_state.clone();
        tokio::spawn(async move {
            run_events_accept_loop(events_listener, server_state_for_events).await;
        });
    }

    // First, sweep "ghost active" rows that the previous engine left
    // behind without ever spawning a worker — `tasks.status = 'active'`
    // with no `work_runs` history at all. These are demoted back to
    // `todo` so `boss chore list --status active` and
    // `bossctl agents list` can't drift apart on the strength of a
    // chore that never reached a slot. Items with run history are
    // left alone for `reconcile_active_dispatch` below to redispatch.
    match server_state.work_db.heal_ghost_active_chores() {
        Ok(healed) if !healed.is_empty() => {
            let ids: Vec<&str> = healed.iter().map(|h| h.work_item_id.as_str()).collect();
            tracing::warn!(
                count = healed.len(),
                ids = ?ids,
                "demoted ghost-active chores with no run history",
            );
            // Publish an invalidation on each owning product topic so
            // subscribed kanban views refetch and move the card out of
            // Doing immediately — without this the engine's demotion
            // stays invisible to the UI until the next manual refresh,
            // which is the silent-divergence half of #680.
            for h in &healed {
                server_state
                    .publisher
                    .publish_work_item_changed(
                        &h.product_id,
                        &h.work_item_id,
                        "ghost-active demotion: dispatch never reached a worker",
                    )
                    .await;
            }
        }
        Ok(_) => {
            tracing::debug!("no ghost-active chores to demote at startup");
        }
        Err(err) => {
            tracing::error!(?err, "ghost-active sweep failed; continuing");
        }
    }

    // Install boss-event to a stable location and heal existing worker
    // settings.json files. This ensures that hook paths baked into worker
    // settings.json survive a `bazel clean` or workspace re-lease.
    //
    // Resolution at install time intentionally skips the stable-bin-dir
    // candidate (pass None) so we always copy the real binary from its
    // original source rather than potentially re-copying a previous install.
    let stable_boss_event_path = {
        let engine_path = std::env::current_exe().unwrap_or_default();
        let workspace_dir = std::env::var_os("BUILD_WORKSPACE_DIRECTORY").map(PathBuf::from);
        let env_override = std::env::var_os("BOSS_EVENT_BIN").map(PathBuf::from);
        let boss_bin_dir = std::env::var_os("BOSS_BIN_DIR").map(PathBuf::from);
        let current_shim = crate::runner::resolve_boss_event_binary(
            &engine_path,
            workspace_dir.as_deref(),
            env_override.as_deref(),
            boss_bin_dir.as_deref(),
            None,
        );
        if let Some(home) = std::env::var_os("HOME") {
            let stable_bin_dir =
                PathBuf::from(home).join("Library/Application Support/Boss/bin");
            match crate::runner::install_boss_event_to_stable_bin(&current_shim, &stable_bin_dir)
            {
                Ok(stable) => {
                    tracing::info!(
                        stable_path = %stable.display(),
                        source_path = %current_shim.display(),
                        "boss-event installed to stable bin dir",
                    );
                    stable
                }
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        source_path = %current_shim.display(),
                        "failed to install boss-event to stable bin dir; \
                         new workers will use the resolved path",
                    );
                    current_shim
                }
            }
        } else {
            current_shim
        }
    };

    // Heal existing worker settings files so a worker whose baked hook
    // path went stale (e.g. after a `bazel clean`) picks up the stable
    // boss-event path on the next engine restart. The settings files
    // live under the system temp dir, outside every workspace — see
    // `worker_setup` module docs.
    let worker_settings_dir = crate::worker_setup::worker_settings_dir();
    tracing::info!(
        dir = %worker_settings_dir.display(),
        new_path = %stable_boss_event_path.display(),
        "healing boss-event path in worker settings files",
    );
    crate::worker_setup::heal_worker_settings_json(
        &worker_settings_dir,
        &stable_boss_event_path,
    );

    // Rehydrate dispatch for any work items that were in "Doing"
    // (status=active) when the engine last shut down but whose
    // executions ended without being moved out of the column. See
    // `tools/boss/docs/designs/work-kanban.md` §3 — the Doing column
    // is supposed to mirror "running or queued," and on startup we
    // re-issue RequestExecution for items that no longer satisfy
    // either half of that contract.
    //
    // On startup the in-memory live-worker registry is empty, so we
    // can't use it as the "is the worker still attached" oracle —
    // taking it at face value would treat every persisted in-flight
    // execution as orphaned and spawn a *second* worker on top of the
    // one already running. That's the duplicate-dispatch bug observed
    // on 2026-05-07 (slot 1+7 / slot 4+8 each on the same chore).
    //
    // Instead, probe `cube workspace list` once and mark every
    // persisted in-flight execution Live / Dead / Unknown based on
    // whether its lease is still bound to the same workspace. The
    // events socket is intentionally NOT consulted (it can be the
    // first thing to break on a crash). See `crate::run_reconcile`
    // for the verdict rules.
    let in_flight = match server_state.work_db.list_in_flight_executions() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(
                ?err,
                "failed to list in-flight executions for startup reconcile; continuing without per-run probe (existing reconcile path may double-dispatch)"
            );
            Vec::new()
        }
    };
    let probe_report = if in_flight.is_empty() {
        tracing::debug!("no persisted in-flight executions to probe at startup");
        crate::run_reconcile::RunReconcileReport::default()
    } else {
        let now_epoch_s = crate::run_reconcile::current_epoch_s();
        let report = crate::run_reconcile::probe_in_flight_runs(
            server_state.cube_client.as_ref(),
            &in_flight,
            now_epoch_s,
        )
        .await;
        tracing::info!(
            in_flight_count = in_flight.len(),
            live = report.live_count,
            dead = report.dead_count,
            unknown = report.unknown_count,
            "engine startup: probed persisted in-flight runs against cube state",
        );
        if report.unknown_count > 0 {
            tracing::warn!(
                unknown = report.unknown_count,
                "startup reconcile produced Unknown verdicts; those work items will NOT be auto-redispatched — operator should investigate"
            );
        }
        report
    };
    let skip_dispatch_ids: HashSet<String> = probe_report
        .skip_dispatch_ids()
        .map(|s| s.to_owned())
        .collect();

    // Reap orphans before reconcile dispatch fires. For every Dead
    // verdict the cube probe returned, mark the execution row
    // `orphaned` (terminal) so the subsequent `reconcile_active_dispatch`
    // sees it as a finished predecessor and inherits its
    // `cube_workspace_id` into the new ready row's
    // `preferred_workspace_id`. The orphan reap intentionally does NOT
    // release the cube workspace lease — the workspace may still hold
    // in-flight commits the next worker should resume against.
    //
    // See docs/post-crash-recovery.md for the full flow.
    let orphan_reason = "engine startup: cube probe verdict Dead — worker lease no longer matches recorded state across restart";
    for (execution_id, verdict) in &probe_report.verdicts {
        if !matches!(verdict, crate::run_reconcile::RunReconcileVerdict::Dead) {
            continue;
        }
        match server_state
            .work_db
            .mark_execution_orphaned(execution_id, orphan_reason)
        {
            Ok(execution) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    cube_workspace_id = ?execution.cube_workspace_id,
                    "startup reaper: marked execution orphaned (workspace preserved for re-lease)",
                );
            }
            Err(err) => {
                // Already-terminal rows are benign here — a parallel
                // sweep (e.g. heal_ghost_active_chores) may have
                // closed the row first. Anything else is real and
                // worth surfacing.
                tracing::warn!(
                    execution_id,
                    error = %format!("{err:#}"),
                    "startup reaper: skipped orphan reap (likely already terminal)",
                );
            }
        }
    }

    match server_state
        .work_db
        .reconcile_active_dispatch(|execution_id| skip_dispatch_ids.contains(execution_id))
    {
        Ok(redispatched) if !redispatched.is_empty() => {
            tracing::info!(
                count = redispatched.len(),
                ids = ?redispatched,
                "reconciled active-dispatch on startup",
            );
        }
        Ok(_) => {
            tracing::debug!("no active-dispatch reconcile needed at startup");
        }
        Err(err) => {
            tracing::error!(?err, "active-dispatch reconcile failed; continuing");
        }
    }

    // Backfill execution_request rows for any conflict_resolutions
    // attempts that were inserted before PR #430 wired the
    // create_execution call into on_conflict_detected. Idempotent: a
    // second run finds zero orphans and fast-returns.
    crate::conflict_watch::backfill_orphaned_executions(
        &server_state.work_db,
        server_state.publisher.as_ref(),
    );

    // Spawn the merge-detection poller. Workers can land their PRs
    // long after their Stop event has fired (and lease has been
    // released), so the on-Stop completion path can't catch every
    // merge. The poller fills that gap by periodically asking GitHub
    // about every chore that's currently in_review with a pr_url and
    // promoting the merged ones to `done`. Polling cadence is
    // deliberately conservative — chores rarely sit in review for
    // long, and we don't want to spam `gh` from the engine process.
    let merge_probe: Arc<dyn MergeProbe> = Arc::new(CommandMergeProbe::new());
    let _merge_handle = spawn_merge_poller(
        server_state.work_db.clone(),
        merge_probe,
        server_state.publisher.clone(),
        server_state.cube_client.clone(),
        server_state.completion_handler.clone(),
        Duration::from_secs(60),
        server_state.metrics.clone(),
        server_state.pr_reconciler_kick.clone(),
    );

    // Periodic dead-PID reconciler: detects worker slots whose backing
    // OS process has died (kill-9, crash, OOM) and reaps them so the
    // orphan sweep can redispatch the chore. Runs every 60s and fires
    // immediately on boot. Without this, a kill-9'd worker leaves the
    // pool slot claimed forever and the orphan sweep skips the chore
    // ("already claimed"), leaving it stuck in Doing indefinitely.
    let _dead_pid_sweep_handle = crate::dead_pid_sweep::spawn_loop(
        server_state.work_db.clone(),
        server_state.live_worker_states.clone(),
        server_state.execution_coordinator.clone(),
        server_state.dispatch_events.clone(),
        Duration::from_secs(60),
    );

    // Periodic transient-recovery reconciler: detects workers wedged by
    // a transient Claude API error (the interactive `claude` session
    // printed the error, ended its turn, and sits Idle while the chore
    // is unfinished) and auto-resumes them on the same workspace with
    // bounded retries + backoff, escalating non-retryable / cap-reached
    // failures for human attention. Runs every 60s and fires on boot.
    let _transient_recovery_handle = crate::transient_recovery::spawn_loop(
        server_state.work_db.clone(),
        server_state.live_worker_states.clone(),
        server_state.execution_coordinator.clone(),
        server_state.dispatch_events.clone(),
        Arc::clone(&server_state) as Arc<dyn crate::transient_recovery::WorkerNudger>,
        crate::transient_recovery::DEFAULT_INTERVAL,
    );

    // Periodic orphan-active reconciler: re-dispatches `active` work
    // items that have no live execution (the post-crash "stuck-in-Doing"
    // fix). Runs every 60s and fires immediately on boot so items left
    // orphaned by the previous crash are recovered without waiting for
    // the first interval.
    let _orphan_sweep_handle = crate::orphan_sweep::spawn_loop(
        server_state.work_db.clone(),
        server_state.execution_coordinator.clone(),
        server_state.dispatch_events.clone(),
        Duration::from_secs(60),
    );

    // External-tracker reconciler: periodically pulls upstream issue state
    // into Boss's work-item taxonomy. Default cadence: 120 s (2 min) per
    // the design doc's §"Cadence" rationale (Design Q5). Fires immediately
    // on spawn so any drift accumulated while the engine was offline is
    // reconciled at boot without waiting for the first interval.
    let _external_tracker_handle =
        crate::external_tracker::reconcile::spawn_loop(
            server_state.work_db.clone(),
            server_state.tracker_registry.clone(),
            Duration::from_secs(120),
            server_state.metrics.clone(),
            server_state.clone(),
        );

    // Dependency-unblock safety-net sweeper: periodically re-evaluates
    // every dependency-blocked work item and unblocks any whose gating
    // prerequisites have all reached a satisfied status. The primary
    // unblock path is event-driven (cascade inside the prereq-done
    // transaction), but that path can silently skip a row if the item's
    // last_status_actor was reset between the auto-block and the prereq
    // landing, or if the engine was offline at transition time. The
    // sweeper recovers those cases within one interval (≤30 s).
    // See dep_unblock_sweep.rs for the full incident trace.
    let coord_for_dep_unblock = server_state.execution_coordinator.clone();
    let _dep_unblock_handle = crate::dep_unblock_sweep::spawn_loop(
        server_state.work_db.clone(),
        Duration::from_secs(crate::dep_unblock_sweep::DEP_UNBLOCK_SWEEP_INTERVAL_SECS),
        server_state.metrics.clone(),
        Arc::new(move || coord_for_dep_unblock.kick()),
    );

    // Scheduler heartbeat: periodic `kick()` so a ready row stranded
    // by a dropped wakeup (the `status_transition` → `request_recorded`
    // stall class — see `exec_18af3ba5259d32a8_12`, 2026-05-13) is
    // picked up within one interval instead of waiting for the 90s
    // orphan-active reconciler. Logs a `warn!` when a stranded row is
    // observed so an operator notices the dropped wakeup on the first
    // occurrence rather than only inferring it from the redispatch
    // event. PR #429's reconciler remains the safety net for execution
    // rows whose worker has died — the heartbeat only re-kicks the
    // scheduler, it does not abandon or insert rows.
    let _scheduler_heartbeat_handle = server_state
        .execution_coordinator
        .spawn_scheduler_heartbeat(Duration::from_secs(15));

    // Watch in-flight dispatch timelines for stalled stages and emit
    // a `stage_stalled` event when one sits past the threshold
    // without progressing. Read-only against the per-execution
    // dispatch.jsonl mirrors; never modifies dispatcher behavior.
    //
    // Per-stage overrides: the early dispatch handoffs (worker
    // claim → cube repo ensure → cube workspace lease) should
    // never sit for more than ~30s in healthy operation, so flag
    // them faster than the 120s default. The 2026-05-12 cube-lease
    // hang spent 46s in `worker_claimed` with no event firing
    // because the global threshold hadn't elapsed; a 30s override
    // catches it on the first sweep after the wedge.
    let stage_thresholds =
        crate::dispatch_reader::StageThresholds::new(Duration::from_secs(120))
            .with_override("worker_claimed", Duration::from_secs(30))
            .with_override("cube_repo_ensured", Duration::from_secs(60))
            .with_override("cube_workspace_lease_attempted", Duration::from_secs(30));
    let _stage_stalled_handle = crate::dispatch_reader::spawn_stage_stalled_detector(
        server_state.dispatch_event_root.clone(),
        server_state.dispatch_events.clone(),
        stage_thresholds,
        Duration::from_secs(15),
    );

    // Periodic metrics flush — snapshots the in-memory counter /
    // gauge registry into `state.db` every 30s so monotonic totals
    // survive engine restarts (see
    // `tools/boss/docs/designs/engine-counter-metrics-framework.md`
    // §"Persistence: state.db table"). The graceful-shutdown branch
    // below runs one final flush before returning so the last 0–30s
    // window of increments isn't lost on a normal exit.
    let _metrics_flush_handle = crate::metrics::spawn_flush_task(
        server_state.metrics.clone(),
        server_state.work_db.clone(),
    );

    let coordinator = server_state.execution_coordinator.clone();
    coordinator.kick();

    install_panic_hook(&server_state);

    // Orphan watcher: poll the watched parent pid every second.  When it's
    // gone (the bazel test runner that spawned us exited), fire a notify so
    // the accept loop below can exit cleanly instead of becoming a
    // long-lived orphan that holds production sockets / DB / pid file.
    // Only armed for test-fixture engines (watched_parent_pid is Some).
    let orphan_trigger = Arc::new(Notify::new());
    if let Some(ppid) = watched_parent_pid {
        let trigger = orphan_trigger.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if !process_is_alive(ppid) {
                    tracing::warn!(
                        parent_pid = ppid,
                        "parent process exited — test-fixture engine orphaned; exiting cleanly"
                    );
                    trigger.notify_one();
                    break;
                }
            }
        });
    }

    tracing::info!(
        socket_path = %socket_path.display(),
        "frontend socket: accept loop started",
    );
    crate::audit::record_accept_loop_started("frontend", &socket_path);

    let shutdown_trigger_for_loop = server_state.shutdown_trigger.clone();
    let orphan_trigger_for_loop = orphan_trigger.clone();
    loop {
        tokio::select! {
            biased;
            signal = graceful_shutdown_signal() => {
                tracing::info!(signal, "shutdown signal received; releasing worker panes");
                crate::audit::record_shutdown(format!("signal:{signal}"));
                server_state
                    .shutdown_workers(Duration::from_secs(5), Duration::from_secs(1))
                    .await;
                // One final metrics flush so the 0–30s window of
                // increments between the last periodic flush and the
                // shutdown signal isn't dropped on a clean exit.
                // Crash-loss is acceptable for monotonic counts; a
                // clean exit can afford to do better.
                if let Err(err) = crate::metrics::flush_all(
                    &server_state.metrics,
                    &server_state.work_db,
                ) {
                    tracing::warn!(?err, "metrics: final flush on shutdown failed");
                }
                tracing::info!("engine shutdown complete");
                return Ok(());
            }
            _ = shutdown_trigger_for_loop.notified() => {
                tracing::info!("shutdown rpc accepted; releasing worker panes");
                crate::audit::record_shutdown("rpc");
                server_state
                    .shutdown_workers(Duration::from_secs(5), Duration::from_secs(1))
                    .await;
                if let Err(err) = crate::metrics::flush_all(
                    &server_state.metrics,
                    &server_state.work_db,
                ) {
                    tracing::warn!(?err, "metrics: final flush on shutdown failed");
                }
                tracing::info!("engine shutdown complete");
                return Ok(());
            }
            _ = orphan_trigger_for_loop.notified() => {
                tracing::info!("orphan shutdown: test-fixture parent is gone; exiting");
                crate::audit::record_shutdown("orphan");
                return Ok(());
            }
            accept = listener.accept() => {
                let (stream, _) = accept.context("socket accept failed")?;
                // Capture peer pid synchronously before any async yield so the
                // shim's quick-close (or any other peer that doesn't linger)
                // can't race us into ENOTCONN.
                let peer_pid_value = peer_pid(&stream).ok();
                let server_state = server_state.clone();
                tokio::spawn(async move {
                    if let Err(err) =
                        handle_frontend_connection(stream, server_state, peer_pid_value).await
                    {
                        tracing::error!(?err, "frontend connection failed");
                    }
                });
            }
        }
    }
}

/// Constant-time byte comparison. Used by the shutdown-RPC token
/// gate so a wrong-length or wrong-content token can't be inferred
/// from response timing — the same costs as the real comparison,
/// regardless of where the mismatch lands.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Walk up `pid`'s process tree (bounded depth) checking whether
/// any ancestor matches one of `trust_roots`. Used to implement
/// `LOCAL_PEERPID` subtree-match auth: a peer running inside a
/// trusted process tree is treated as that tree's tier.
fn is_descendant_of_any(pid: libc::pid_t, trust_roots: &[libc::pid_t]) -> bool {
    use crate::worker_registry::parent_pid;
    const TRUST_WALK_DEPTH: usize = 16;
    let mut current = pid;
    for _ in 0..TRUST_WALK_DEPTH {
        if trust_roots.contains(&current) {
            return true;
        }
        match parent_pid(current) {
            Ok(Some(parent)) => current = parent,
            Ok(None) | Err(_) => return false,
        }
    }
    false
}

/// Whether `pid` names a live process. Implemented with `kill(pid, 0)`,
/// which delivers no signal but performs the existence + permission
/// check: `Ok` means the process exists, `EPERM` means it exists but is
/// owned by another user (still alive), and `ESRCH` means no such
/// process. Used by `RegisterAppSession` to decide whether a stale app
/// trust root can be superseded by a relaunched app — only when the old
/// app process is genuinely gone.
fn pid_is_alive(pid: libc::pid_t) -> bool {
    // Reject pid <= 0: `kill(0, _)` targets the caller's process group
    // and `kill(-pid, _)` a process group, neither of which is the
    // single-process liveness probe we want — interpreting their result
    // as "alive" would be wrong.
    if pid <= 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 performs no action beyond the
    // existence/permission probe; we only read `errno` on failure.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Decide whether a `RegisterAppSession` from `peer_pid` should be
/// trusted, given the currently-pinned app trust root `current_app_pid`
/// and the engine's own pid. Extracted from the connection handler so
/// the trust transitions (matching pid, engine-ancestor, dead-old-app
/// reattach) are unit-testable. See the call site for the rationale of
/// each branch.
fn register_app_session_trust_ok(
    current_app_pid: Option<libc::pid_t>,
    peer_pid: Option<libc::pid_t>,
    engine_pid: libc::pid_t,
) -> bool {
    match (current_app_pid, peer_pid) {
        (None, _) => true, // tests / no-trust-root mode
        (Some(expected), Some(observed)) => {
            observed == expected
                || is_descendant_of_any(engine_pid, &[observed])
                || !pid_is_alive(expected)
        }
        (Some(_), None) => false,
    }
}

/// Resolve the `last_status_actor` string for an RPC-driven status change.
///
/// Returns `"boss"` when the caller's process ancestry matches the registered
/// Boss-coordinator session pid; `"human"` otherwise. Engine-internal writers
/// stamp `"engine"` directly in SQL and never call this function.
fn resolve_status_actor(server_state: &ServerState, peer_pid: Option<libc::pid_t>) -> &'static str {
    let boss_pid = server_state.current_boss_pid();
    if let (Some(boss_pid), Some(peer_pid)) = (boss_pid, peer_pid) {
        if is_descendant_of_any(peer_pid, &[boss_pid]) {
            return boss_protocol::LAST_STATUS_ACTOR_BOSS;
        }
    }
    boss_protocol::LAST_STATUS_ACTOR_HUMAN
}

fn current_parent_pid() -> Option<libc::pid_t> {
    // BOSS_APP_PID is the only signal we trust to identify the app
    // tier. The macOS app sets it to its own pid before spawning the
    // engine — necessary because `bazel run` daemonizes its server,
    // reparenting the engine away from the app's process tree, so
    // `getppid()` lands on `bazel` (or launchd) instead of the app.
    //
    // When BOSS_APP_PID is unset we leave app_pid as None rather than
    // guessing from `getppid()`. Falling back to the parent yields a
    // wrong-but-confident answer in every dev setup that launches the
    // engine independently of the app (`bazel run` from a terminal,
    // direct invocation of the binary, etc.) — the engine pins its
    // trust root to bazel/launchd and then rejects every legitimate
    // `RegisterAppSession` from the real app, which kills dispatch
    // (every `SpawnWorkerPane` request fails because no app session
    // is registered to receive it). With None, the trust gate becomes
    // a no-op (matches the test path), the app registers, and the
    // Boss session pid takes over as the real trust root once
    // `RegisterBossSession` lands. Production is unaffected: the app
    // always sets BOSS_APP_PID via `EngineProcessController`.
    std::env::var("BOSS_APP_PID")
        .ok()
        .and_then(|raw| raw.parse::<libc::pid_t>().ok())
        .filter(|&pid| pid > 1)
}

/// Send `SIGTERM` to every pid in `pids`, sleep `grace`, then send
/// `SIGKILL` to anything still alive. Used as the shutdown fallback
/// when the app teardown path didn't release the worker shell — and
/// from the panic hook, where we must not touch the runtime. The
/// loop keeps going past `EPERM` / `ESRCH` because the worker may
/// already be dead (good) or owned by another uid (we can't help).
fn signal_shell_pids(pids: &[libc::pid_t], grace: Duration) {
    if pids.is_empty() {
        return;
    }
    for &pid in pids {
        // SAFETY: `kill` with a pid we recorded ourselves; failure is
        // logged but not fatal.
        let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
        if rc != 0 {
            tracing::debug!(
                pid,
                errno = std::io::Error::last_os_error().raw_os_error(),
                "shutdown_workers: SIGTERM returned non-zero (likely already exited)",
            );
        }
    }
    if grace > Duration::from_secs(0) {
        std::thread::sleep(grace);
    }
    for &pid in pids {
        // SAFETY: same as above.
        let rc = unsafe { libc::kill(pid, libc::SIGKILL) };
        if rc != 0 {
            tracing::debug!(
                pid,
                errno = std::io::Error::last_os_error().raw_os_error(),
                "shutdown_workers: SIGKILL returned non-zero",
            );
        }
    }
}

/// Snapshot of the (slot_id, shell_pid) pairs currently registered as
/// live workers, intended for the panic-hook path: pulls just enough
/// state to fire `SIGTERM`/`SIGKILL` without touching the runtime,
/// async, or Tokio internals (any of which could deadlock during
/// unwind).
fn snapshot_live_shell_pids(server_state: &ServerState) -> Vec<libc::pid_t> {
    server_state
        .live_worker_states
        .snapshot()
        .into_iter()
        .filter_map(|s| (s.shell_pid > 0).then_some(s.shell_pid as libc::pid_t))
        .collect()
}

/// Install a panic hook that emergency-kills every recorded worker
/// shell pid before delegating to the previously-installed hook. The
/// async `release_worker_pane` path is unsafe inside an unwinding
/// runtime — we settle for the synchronous SIGTERM/SIGKILL fallback
/// so the worker tree doesn't outlive the engine.
///
/// We hold a `Weak` so the hook never keeps `ServerState` alive past
/// a normal shutdown.
fn install_panic_hook(server_state: &Arc<ServerState>) {
    let weak = Arc::downgrade(server_state);
    let prior = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(server) = weak.upgrade() {
            let pids = snapshot_live_shell_pids(&server);
            if !pids.is_empty() {
                tracing::error!(
                    count = pids.len(),
                    "engine panic: emergency-killing worker shells before unwind",
                );
                signal_shell_pids(&pids, Duration::from_millis(500));
            }
        }
        prior(info);
    }));
}

/// Future that resolves when a graceful-shutdown signal arrives
/// (`SIGINT` or `SIGTERM`). Resolves to a static label naming which
/// signal fired so the caller can log it.
async fn graceful_shutdown_signal() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(err) => {
            tracing::error!(?err, "failed to install SIGTERM handler; only SIGINT will trigger graceful shutdown");
            tokio::signal::ctrl_c().await.ok();
            return "SIGINT";
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => "SIGINT",
        _ = sigterm.recv() => "SIGTERM",
    }
}

async fn run_events_accept_loop(listener: UnixListener, server_state: Arc<ServerState>) {
    let local_addr = listener.local_addr().ok();
    let path_display = local_addr
        .as_ref()
        .and_then(|a| a.as_pathname())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<unknown>".to_owned());
    tracing::info!(
        events_socket_path = %path_display,
        "events socket: accept loop started",
    );
    if let Some(p) = local_addr.as_ref().and_then(|a| a.as_pathname()) {
        crate::audit::record_accept_loop_started("events", p);
    }
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let server_state = server_state.clone();
                tokio::spawn(async move {
                    match handle_connection(stream).await {
                        Ok(incoming) => {
                            tracing::info!(
                                peer_pid = ?incoming.peer_pid,
                                run_id = ?incoming.run_id,
                                event = ?incoming.event,
                                "events socket: hook event received",
                            );
                            // Audit *before* the live-state fan-out
                            // so an engine-side mismatch in the
                            // dispatch path can't drop the audit line
                            // — the deny is enforced harness-side by
                            // claude already, this is the independent
                            // forensic record. See
                            // [`worker_sandbox_audit`] for why.
                            crate::worker_sandbox_audit::record_if_sandbox_attempt(
                                &server_state.dispatch_event_root,
                                incoming.run_id.as_deref(),
                                &incoming.event,
                            );
                            dispatch_live_worker_state(&server_state, &incoming).await;
                            // Urgent probes fire on PostToolUse so
                            // the coordinator can redirect a worker
                            // mid-task without waiting for Stop. The
                            // tool call has already returned at this
                            // point, so no in-flight work is lost.
                            dispatch_urgent_probe_on_post_tool_use(&server_state, &incoming).await;
                            // ProbeReplied runs first: emit the reply for the
                            // prior probe before dispatching the next one so
                            // a single Stop never fires both reply and dispatch
                            // for the same probe (the reply text hasn't been
                            // written yet at dispatch time).
                            //
                            // Completion runs before probe dispatch: probes
                            // queued by the completion handler (e.g.
                            // PROBE_NO_PR) must be visible to `dispatch_probe_on_stop`
                            // so they are delivered on the *same* Stop that
                            // triggered them rather than stalling until the
                            // next Stop (which never comes for an idle worker).
                            dispatch_probe_reply_on_stop(&server_state, &incoming).await;
                            dispatch_completion_on_stop(&server_state, &incoming).await;
                            dispatch_probe_on_stop(&server_state, &incoming).await;
                        }
                        Err(err) => {
                            tracing::warn!(?err, "events socket: failed to handle connection");
                        }
                    }
                });
            }
            Err(err) => {
                tracing::error!(?err, "events socket accept failed");
            }
        }
    }
}

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

async fn dispatch_live_worker_state(
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
        server_state
            .dispatcher_stats
            .inc_dropped_missing_run_id();
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
            let _ = server_state.transcript_path_cache.record_if_unset(run_id, path);
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
    let Some(slot_id) = server_state.worker_registry.slot_for_run(run_id) else {
        tracing::warn!(
            run_id,
            kind = event_kind,
            "live_status: dropping hook fan-out — run_id is not registered against a slot (event ahead of register_run_slot or after take_slot_for_run); transcript_path already persisted",
        );
        return;
    };
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
            server_state
                .live_status_manager
                .notify(slot_id, Trigger::Stop);
        }
        crate::protocol::WorkerEvent::PostToolUse {
            tool_name,
            tool_input,
            tool_response,
            ..
        } => {
            server_state
                .live_status_manager
                .notify(slot_id, Trigger::PostToolUse);
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
                        let outcome = server_state
                            .staged_pr_urls
                            .record_if_unset(run_id, &pr_url);
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
            // Conflict-resolution primary-path signal capture. For
            // executions of kind `conflict_resolution`, detect force-push
            // and resolution-comment events and stage them so `on_stop`
            // can transition the parent chore without a merge-poller sweep.
            if tool_name == "Bash" {
                let is_conflict_resolution = server_state
                    .work_db
                    .get_execution(run_id)
                    .map(|e| e.kind == "conflict_resolution")
                    .unwrap_or(false);
                if is_conflict_resolution {
                    if crate::resolution_signal_capture::is_force_push_command(tool_input) {
                        server_state.staged_resolution_signals.record_signal(
                            run_id,
                            crate::resolution_signal_capture::ResolutionSignal::ForcePushed,
                        );
                        tracing::info!(
                            execution_id = run_id,
                            "resolution_signal_capture: staged ForcePushed signal",
                        );
                    }
                    if let Some(comment_url) =
                        crate::resolution_signal_capture::extract_resolution_comment_url(
                            tool_response,
                        )
                    {
                        server_state.staged_resolution_signals.record_signal(
                            run_id,
                            crate::resolution_signal_capture::ResolutionSignal::ResolutionCommentPosted,
                        );
                        tracing::info!(
                            execution_id = run_id,
                            %comment_url,
                            "resolution_signal_capture: staged ResolutionCommentPosted signal",
                        );
                    }
                }
            }
        }
        _ => {}
    }
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

/// On `Stop` hook events, pop a pending probe for the run (if any)
/// and `SendToPane` the text to the worker's slot. The injection
/// arrives at the pane just as the worker becomes idle, so claude
/// treats it as the next user prompt. After a successful dispatch,
/// records an in-flight entry (with the transcript path and current
/// byte offset) so `dispatch_probe_reply_on_stop` can emit the
/// matching `FrontendEvent::ProbeReplied` when the next Stop lands.
async fn dispatch_probe_on_stop(
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
async fn dispatch_urgent_probe_on_post_tool_use(
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
async fn dispatch_probe_if_idle(server_state: &Arc<ServerState>, run_id: &str) {
    use crate::protocol::{EngineToAppRequest, SendToPaneInput};
    use boss_protocol::WorkerActivity;

    let Some(slot_id) = server_state.worker_registry.slot_for_run(run_id) else {
        // Worker not yet mapped to a slot (spawning) — probe stays queued.
        tracing::debug!(run_id, "probe-if-idle: no slot mapping; probe waits for Stop");
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
    let path = match server_state
        .work_db
        .transcript_path_for_execution(run_id)
    {
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
async fn dispatch_probe_reply_on_stop(
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
fn extract_last_assistant_text(chunk: &str) -> Option<String> {
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
async fn dispatch_completion_on_stop(
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

        match request {
            FrontendRequest::Subscribe { topics } => {
                let topics = server_state
                    .topic_broker
                    .subscribe(&session_id, &topics)
                    .await;
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::Subscribed {
                        topics,
                        current_revision: server_state.current_work_revision(),
                    },
                );
            }
            FrontendRequest::Unsubscribe { topics } => {
                let topics = server_state
                    .topic_broker
                    .unsubscribe(&session_id, &topics)
                    .await;
                send_response(&sink, &request_id, FrontendEvent::Unsubscribed { topics });
            }
            FrontendRequest::CreateProduct { input } => match work_db.create_product(input) {
                Ok(product) => {
                    let item = WorkItem::Product(product);
                    let revision = publish_work_invalidation(
                        &server_state,
                        &session_id,
                        &request_id,
                        vec![
                            TOPIC_WORK_PRODUCTS.to_owned(),
                            work_product_topic(&work_item_id(&item)),
                        ],
                        "product_created",
                        Some(work_item_product_id(&item)),
                        vec![work_item_id(&item)],
                    )
                    .await;
                    send_response_with_revision(
                        &sink,
                        &request_id,
                        revision,
                        FrontendEvent::WorkItemCreated { item },
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
            },
            FrontendRequest::ListProducts => match work_db.list_products() {
                Ok(products) => {
                    send_response_with_revision(
                        &sink,
                        &request_id,
                        server_state.current_work_revision(),
                        FrontendEvent::ProductsList { products },
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
            },
            FrontendRequest::ListProjects {
                product_id,
                dep_filter,
            } => {
                match work_db.list_projects(&product_id, dep_filter.as_ref()) {
                    Ok(projects) => {
                        send_response_with_revision(
                            &sink,
                            &request_id,
                            server_state.current_work_revision(),
                            FrontendEvent::ProjectsList {
                                product_id,
                                projects,
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
            FrontendRequest::ListTasks {
                product_id,
                project_id,
                dep_filter,
            } => match work_db.list_tasks(&product_id, project_id.as_deref(), dep_filter.as_ref()) {
                Ok(tasks) => {
                    send_response_with_revision(
                        &sink,
                        &request_id,
                        server_state.current_work_revision(),
                        FrontendEvent::TasksList {
                            product_id,
                            project_id,
                            tasks,
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
            },
            FrontendRequest::ListChores {
                product_id,
                dep_filter,
            } => match work_db.list_chores(&product_id, dep_filter.as_ref()) {
                Ok(chores) => {
                    send_response_with_revision(
                        &sink,
                        &request_id,
                        server_state.current_work_revision(),
                        FrontendEvent::ChoresList { product_id, chores },
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
            },
            FrontendRequest::GetWorkItem { id } => {
                // Use resolving variant so callers can pass T-form short ids
                // (e.g. `T688`) without knowing the product; the DB lookup is
                // global and short ids are unique across all products.
                let result = work_db
                    .get_work_item_resolving_short_id(&id)
                    .and_then(|opt| {
                        opt.ok_or_else(|| anyhow::anyhow!("unknown work item: {id}"))
                    });
                match result {
                    Ok(item) => {
                        send_response_with_revision(
                            &sink,
                            &request_id,
                            server_state.current_work_revision(),
                            FrontendEvent::WorkItemResult { item },
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
            FrontendRequest::GetWorkItemByShortId {
                product_id,
                short_id,
            } => match work_db.get_work_item_by_short_id(&product_id, short_id) {
                Ok(Some(item)) => {
                    send_response_with_revision(
                        &sink,
                        &request_id,
                        server_state.current_work_revision(),
                        FrontendEvent::WorkItemResult { item },
                    );
                }
                Ok(None) => {
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: format!(
                                "no work item with id #{short_id} in product {product_id}"
                            ),
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
            },
            FrontendRequest::CreateProject { input } => match work_db.create_project(input) {
                Ok(project) => {
                    let item = WorkItem::Project(project);
                    let product_id = work_item_product_id(&item);
                    let revision = publish_work_invalidation(
                        &server_state,
                        &session_id,
                        &request_id,
                        vec![work_product_topic(&product_id)],
                        "project_created",
                        Some(product_id),
                        vec![work_item_id(&item)],
                    )
                    .await;
                    send_response_with_revision(
                        &sink,
                        &request_id,
                        revision,
                        FrontendEvent::WorkItemCreated { item },
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
            },
            FrontendRequest::CreateTask { mut input } => {
                if input.created_via.is_none() {
                    input.created_via =
                        Some(transport_default_created_via(&server_state, &session_id).await);
                }
                match work_db.create_task(input) {
                Ok(task) => {
                    let item = WorkItem::Task(task);
                    let product_id = work_item_product_id(&item);
                    let revision = publish_work_invalidation(
                        &server_state,
                        &session_id,
                        &request_id,
                        vec![work_product_topic(&product_id)],
                        "task_created",
                        Some(product_id),
                        vec![work_item_id(&item)],
                    )
                    .await;
                    send_response_with_revision(
                        &sink,
                        &request_id,
                        revision,
                        FrontendEvent::WorkItemCreated { item },
                    );
                }
                Err(err) => {
                    send_response(
                        &sink,
                        &request_id,
                        duplicate_or_work_error(err),
                    );
                }
            }
            }
            FrontendRequest::CreateChore { mut input } => {
                if input.created_via.is_none() {
                    input.created_via =
                        Some(transport_default_created_via(&server_state, &session_id).await);
                }
                match work_db.create_chore(input) {
                Ok(task) => {
                    let item = WorkItem::Chore(task);
                    let product_id = work_item_product_id(&item);
                    let revision = publish_work_invalidation(
                        &server_state,
                        &session_id,
                        &request_id,
                        vec![work_product_topic(&product_id)],
                        "chore_created",
                        Some(product_id),
                        vec![work_item_id(&item)],
                    )
                    .await;
                    send_response_with_revision(
                        &sink,
                        &request_id,
                        revision,
                        FrontendEvent::WorkItemCreated { item },
                    );
                }
                Err(err) => {
                    send_response(
                        &sink,
                        &request_id,
                        duplicate_or_work_error(err),
                    );
                }
            }
            }
            FrontendRequest::CreateManyTasks { mut input } => {
                let fallback = transport_default_created_via(&server_state, &session_id).await;
                for item in &mut input.items {
                    if item.created_via.is_none() {
                        item.created_via = Some(fallback.clone());
                    }
                }
                handle_create_many(
                    work_db.create_many_tasks(input),
                    "tasks_created",
                    WorkItem::Task,
                    &server_state,
                    &session_id,
                    &request_id,
                    &sink,
                )
                .await;
            }
            FrontendRequest::CreateManyChores { mut input } => {
                let fallback = transport_default_created_via(&server_state, &session_id).await;
                for item in &mut input.items {
                    if item.created_via.is_none() {
                        item.created_via = Some(fallback.clone());
                    }
                }
                handle_create_many(
                    work_db.create_many_chores(input),
                    "chores_created",
                    WorkItem::Chore,
                    &server_state,
                    &session_id,
                    &request_id,
                    &sink,
                )
                .await;
            }
            FrontendRequest::UpdateWorkItem { id, patch } => {
                // Capture the task/chore status before the update so we
                // can detect a transition into `active` after the patch
                // applies. We only care about task/chore — products and
                // projects have no execution lifecycle.
                let previous_task_status = task_status_for_id(&work_db, &id);
                // Capture name+description before the update so the
                // chore-update worker notification can report old → new.
                // Only read when the patch touches these fields to avoid
                // an unconditional DB round-trip on status-only patches.
                let previous_spec = if patch.name.is_some() || patch.description.is_some() {
                    task_name_description_for_id(&work_db, &id)
                } else {
                    None
                };
                // Bug #679: when the patch is a kanban drag-to-Doing
                // (a task/chore transitioning from non-active to
                // `active`) and dispatch would deterministically fail
                // because the row has no resolvable repo, reject the
                // `UpdateWorkItem` outright instead of letting the
                // status flip land and then swallowing the dispatch
                // error in a `WARN`. The card stays in its previous
                // column and the user sees a `WorkError` toast naming
                // the missing repo. Skips when an existing non-terminal
                // execution would already own the dispatch slot —
                // there's no point validating a code path we won't run.
                let intends_active_transition = patch.status.as_deref() == Some("active")
                    && previous_task_status
                        .as_deref()
                        .is_some_and(|prev| prev != "active");
                if intends_active_transition && work_item_needs_dispatch(&work_db, &id) {
                    if let Err(err) = work_db.precheck_dispatch_repo(&id) {
                        let work_item_id_for_event = id.clone();
                        let from_status = previous_task_status.clone();
                        let error_message = format!("{err:#}");
                        let details = serde_json::json!({
                            "from_status": from_status,
                            "to_status": "active",
                            "did_dispatch": false,
                            "rejected": true,
                            "reason_if_skipped": error_message,
                            "dispatched_execution_id": serde_json::Value::Null,
                        });
                        server_state
                            .dispatch_events
                            .emit(
                                crate::dispatch_events::DispatchEvent::new(
                                    crate::dispatch_events::Stage::StatusTransition,
                                    crate::dispatch_events::Outcome::Error,
                                    work_item_id_for_event.clone(),
                                )
                                .with_work_item(work_item_id_for_event)
                                .with_error(&err)
                                .with_details(details),
                            )
                            .await;
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: err.to_string(),
                            },
                        );
                        continue;
                    }
                }
                let actor = resolve_status_actor(&server_state, peer_pid);
                match work_db.update_work_item_as_actor(&id, patch, actor) {
                    Ok(item) => {
                        let product_id = work_item_product_id(&item);
                        let mut topics = vec![work_product_topic(&product_id)];
                        if matches!(item, WorkItem::Product(_)) {
                            topics.push(TOPIC_WORK_PRODUCTS.to_owned());
                        }
                        // If the patch moved a task/chore into a
                        // terminal status (`done`, `archived`, or
                        // `cancelled`), tear down whatever resources
                        // its latest execution still holds: the
                        // libghostty pane and the cube workspace.
                        // Idempotent — duplicate or no-op cases
                        // (already released, never spawned, not a
                        // task/chore) collapse inside force_release.
                        if let Some(execution_id) = terminal_chore_execution(&work_db, &item) {
                            let handler = server_state.completion_handler.clone();
                            tokio::spawn(async move {
                                handler.force_release(&execution_id).await;
                            });
                        }
                        // If the patch moved a task/chore into
                        // `in_review`, release pane + cube workspace
                        // for the same reason — the worker is done
                        // with the slot. The worker auto-transition
                        // path (Stop hook → finalize_pr_transition)
                        // handles its own release; this block covers
                        // the human-drag path and any ghost panes left
                        // behind by a failed or partial auto-release.
                        // Idempotent for the same reasons as above.
                        if let Some(execution_id) = in_review_chore_execution(&work_db, &item) {
                            let handler = server_state.completion_handler.clone();
                            tokio::spawn(async move {
                                handler.force_release(&execution_id).await;
                            });
                        }
                        // Kanban drop-into-Doing (and any other human
                        // path that flips a task/chore to `active` via
                        // UpdateWorkItem) must dispatch a worker — see
                        // `tools/boss/docs/designs/work-kanban.md` §
                        // "Doing column = live or queued". The macOS
                        // client also fires `RequestExecution` after
                        // the status patch, but doing it server-side
                        // closes the gap for older clients (or any
                        // future client that forgets the follow-up
                        // RPC), which is the failure shape the
                        // motivating bug exposed for `autostart=false`
                        // chores parked in `todo`: the autostart gate
                        // blocks creation-time dispatch, so until the
                        // human drags the card there is no execution
                        // at all, and a status flip with no follow-up
                        // RequestExecution leaves an `active` card
                        // with no worker.
                        //
                        // We only create a fresh execution when the
                        // work item has no live/queued one — an
                        // existing non-terminal execution already owns
                        // the dispatch slot, and replacing it would
                        // race the auto-dispatcher (and would void the
                        // execution id the client is already tracking).
                        // The reconcile / rescan paths handle
                        // re-dispatch of stale (worker-died) cases.
                        if task_transitioned_to_active(&previous_task_status, &item) {
                            let work_item_id_for_event = work_item_id(&item);
                            let from_status = previous_task_status.clone();
                            let needs_dispatch =
                                work_item_needs_dispatch(&work_db, &work_item_id_for_event);
                            let (dispatched_execution_id, did_dispatch, skip_reason) =
                                if needs_dispatch {
                                    let live_states = server_state.live_worker_states.clone();
                                    let dispatch_input = RequestExecutionInput {
                                        work_item_id: work_item_id_for_event.clone(),
                                        priority: None,
                                        preferred_workspace_id: None,
                                        force: false,
                                    };
                                    match work_db.request_execution_with_live_check(
                                        dispatch_input,
                                        |run_id| live_states.is_run_live(run_id),
                                    ) {
                                        Ok(execution) => {
                                            server_state.execution_coordinator.kick();
                                            (Some(execution.id), true, None)
                                        }
                                        Err(err) => {
                                            // Deterministic preconditions (no
                                            // resolvable repo, bug #679) are
                                            // caught by the pre-update
                                            // `precheck_dispatch_repo` gate above
                                            // and reject the patch outright. This
                                            // arm now only fires for non-
                                            // deterministic races (e.g., a
                                            // concurrent execution insert lost
                                            // the unique-row gate). Keep the WARN
                                            // so a residual silent skip is still
                                            // observable in engine-trace.jsonl.
                                            tracing::warn!(
                                                work_item_id = %work_item_id_for_event,
                                                ?err,
                                                "UpdateWorkItem → active: auto-dispatch \
                                                 failed; status update kept, no worker spawned",
                                            );
                                            (None, false, Some(format!("{err:#}")))
                                        }
                                    }
                                } else {
                                    // The auto-dispatch gate decided this transition
                                    // already has an in-flight execution. Before this
                                    // event existed the skip was silent — exactly the
                                    // "I dragged it and nothing happened" shape.
                                    (
                                        None,
                                        false,
                                        Some(
                                            "work_item_needs_dispatch=false (existing \
                                             non-terminal execution owns dispatch slot)"
                                                .to_owned(),
                                        ),
                                    )
                                };
                            // Pin the event's execution_id to the resolved exec id
                            // when dispatch landed, falling back to the work item
                            // id otherwise so the line stays correlatable with
                            // anything the operator can grep for.
                            let exec_for_event = dispatched_execution_id
                                .clone()
                                .unwrap_or_else(|| work_item_id_for_event.clone());
                            let details = serde_json::json!({
                                "from_status": from_status,
                                "to_status": "active",
                                "did_dispatch": did_dispatch,
                                "reason_if_skipped": skip_reason,
                                "dispatched_execution_id": dispatched_execution_id,
                            });
                            server_state
                                .dispatch_events
                                .emit(
                                    crate::dispatch_events::DispatchEvent::new(
                                        crate::dispatch_events::Stage::StatusTransition,
                                        if did_dispatch {
                                            crate::dispatch_events::Outcome::Ok
                                        } else {
                                            crate::dispatch_events::Outcome::Skipped
                                        },
                                        exec_for_event,
                                    )
                                    .with_work_item(work_item_id_for_event)
                                    .with_details(details),
                                )
                                .await;
                        }
                        // If the name or description of an active chore
                        // changed, notify the bound worker. The worker may
                        // be mid-flight on the old spec; this notice lets it
                        // adapt without a human manually sending the update.
                        // Fire-and-forget: a failed send (worker pane gone,
                        // app session not registered) must not roll back the
                        // DB update. Two rapid edits may produce two notices
                        // in sequence — that's acceptable per the acceptance
                        // criteria.
                        if let Some((old_name, old_description)) = previous_spec {
                            if let Some(run_id) = active_chore_run_id(&server_state, &item) {
                                let (new_name, new_description) = match &item {
                                    WorkItem::Task(t) | WorkItem::Chore(t) => {
                                        (t.name.clone(), t.description.clone())
                                    }
                                    _ => unreachable!(
                                        "active_chore_run_id only returns Some for tasks/chores"
                                    ),
                                };
                                if let Some(msg) = build_chore_update_message(
                                    &old_name,
                                    &new_name,
                                    &old_description,
                                    &new_description,
                                ) {
                                    let server_for_notify = server_state.clone();
                                    tokio::spawn(async move {
                                        if let Err(err) = server_for_notify
                                            .send_input_to_worker(&run_id, msg)
                                            .await
                                        {
                                            tracing::warn!(
                                                ?err,
                                                %run_id,
                                                "chore-update: failed to notify live worker",
                                            );
                                        }
                                    });
                                }
                            }
                        }
                        let revision = publish_work_invalidation(
                            &server_state,
                            &session_id,
                            &request_id,
                            topics,
                            "work_item_updated",
                            Some(product_id),
                            vec![work_item_id(&item)],
                        )
                        .await;
                        send_response_with_revision(
                            &sink,
                            &request_id,
                            revision,
                            FrontendEvent::WorkItemUpdated { item },
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
            FrontendRequest::DeleteWorkItem { id } => match work_db.get_work_item(&id) {
                Ok(item) => match work_db.delete_work_item(&id) {
                    Ok(()) => {
                        let product_id = work_item_product_id(&item);
                        let revision = publish_work_invalidation(
                            &server_state,
                            &session_id,
                            &request_id,
                            vec![work_product_topic(&product_id)],
                            "work_item_deleted",
                            Some(product_id),
                            vec![work_item_id(&item)],
                        )
                        .await;
                        send_response_with_revision(
                            &sink,
                            &request_id,
                            revision,
                            FrontendEvent::WorkItemDeleted { id },
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
                },
                Err(err) => {
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: err.to_string(),
                        },
                    );
                }
            },
            FrontendRequest::GetWorkTree { product_id } => match work_db.get_work_tree(&product_id)
            {
                Ok(tree) => {
                    send_response_with_revision(
                        &sink,
                        &request_id,
                        server_state.current_work_revision(),
                        FrontendEvent::WorkTree {
                            product: tree.product,
                            projects: tree.projects,
                            tasks: tree.tasks,
                            chores: tree.chores,
                            task_runtimes: tree.task_runtimes,
                            dependencies: tree.dependencies,
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
            },
            FrontendRequest::ReorderProjectTasks {
                project_id,
                task_ids,
            } => match work_db.get_work_item(&project_id) {
                Ok(project_item) => match work_db.reorder_project_tasks(&project_id, &task_ids) {
                    Ok(()) => {
                        let product_id = work_item_product_id(&project_item);
                        let revision = publish_work_invalidation(
                            &server_state,
                            &session_id,
                            &request_id,
                            vec![work_product_topic(&product_id)],
                            "project_tasks_reordered",
                            Some(product_id),
                            task_ids.clone(),
                        )
                        .await;
                        send_response_with_revision(
                            &sink,
                            &request_id,
                            revision,
                            FrontendEvent::ProjectTasksReordered {
                                project_id,
                                task_ids,
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
                },
                Err(err) => {
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: err.to_string(),
                        },
                    );
                }
            },
            FrontendRequest::CreateExecution { input } => match work_db.create_execution(input) {
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
            },
            FrontendRequest::RequestExecution { input } => {
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
                let result = work_db.request_execution_with_live_check(input, |run_id| {
                    live_states.is_run_live(run_id)
                });
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
                                        continue;
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
            FrontendRequest::ListExecutions { work_item_id } => {
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
            FrontendRequest::GetTaskRuntime { work_item_id } => {
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
            FrontendRequest::GetExecution { id } => match work_db.get_execution(&id) {
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
            },
            FrontendRequest::CreateRun { input } => match work_db.create_run(input) {
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
            },
            FrontendRequest::ListRuns { execution_id } => match work_db.list_runs(&execution_id) {
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
            },
            FrontendRequest::GetRun { id } => {
                // Try the run_* namespace first, then fall back to the
                // exec_* namespace. Callers such as `bossctl agents
                // status` pass whatever id they have in hand — often an
                // execution id (exec_*) — but `get_run` joins against
                // `work_runs.id` (run_*), so the lookup silently fails
                // with "unknown run". `list_runs(exec_id)` finds the
                // run via `work_runs.execution_id` and returns the most
                // recent one (the active or last-completed run for that
                // execution).
                let result = work_db.get_run(&id).ok().or_else(|| {
                    work_db
                        .list_runs(&id)
                        .ok()
                        .and_then(|mut runs| runs.pop())
                });
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
            FrontendRequest::CreateAttentionItem { input } => {
                match work_db.create_attention_item(input) {
                    Ok(item) => {
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::AttentionItemCreated { item },
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
            FrontendRequest::ListAttentionItems { execution_id } => {
                match work_db.list_attention_items(&execution_id) {
                    Ok(items) => {
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::AttentionItemsList {
                                execution_id,
                                items,
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
            FrontendRequest::GetAttentionItem { id } => match work_db.get_attention_item(&id) {
                Ok(item) => {
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::AttentionItemResult { item },
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
            },
            FrontendRequest::ListAttentionItemsForWorkItem { work_item_id } => {
                match work_db.list_attention_items_for_work_item(&work_item_id) {
                    Ok(items) => {
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::AttentionItemsForWorkItemList {
                                work_item_id,
                                items,
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
            FrontendRequest::RegisterAppSession => {
                // Trust the peer if any of:
                //   (a) it matches the declared app pid exactly. The
                //       engine reads `BOSS_APP_PID` at startup; the
                //       macOS app sets this before spawning the engine
                //       (necessary because `bazel run` daemonizes,
                //       which severs the engine's process tree from
                //       the app and breaks ancestor-walk auth).
                //   (b) the peer pid appears in the engine's ancestor
                //       chain (covers direct-launch scenarios like
                //       `swift run` where no daemonizing wrapper
                //       exists).
                //   (c) APP RESTART against a surviving engine: the
                //       trusted app pid belongs to a now-dead process
                //       and a fresh app instance is connecting. The
                //       engine correctly stays up on a same-version
                //       relaunch, so the relaunched app must be able to
                //       re-attach its session — otherwise the stale pid
                //       rejects `RegisterAppSession` forever, no
                //       `app_session` is registered, and every
                //       engine→app RPC (`SpawnWorkerPane`, reveal) dies
                //       silently. This is the mirror of T351 (engine
                //       restart re-attaching surviving panes): there the
                //       app survives and the engine restarts; here the
                //       engine survives and the app restarts. We require
                //       the old pid to be genuinely dead so a second
                //       live app can't hijack the trust root from the
                //       real one.
                let engine_pid = std::process::id() as libc::pid_t;
                let current_app_pid = server_state.current_app_pid();
                let trust_ok =
                    register_app_session_trust_ok(current_app_pid, peer_pid, engine_pid);
                if !trust_ok {
                    tracing::warn!(
                        peer_pid = ?peer_pid,
                        engine_pid,
                        expected_app_pid = ?current_app_pid,
                        "register_app_session rejected: peer pid neither matches BOSS_APP_PID nor is an engine ancestor",
                    );
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::Error {
                            message: "register_app_session: peer pid does not match app_pid"
                                .to_owned(),
                        },
                    );
                    continue;
                }
                // Re-pin the trust root to the (re)connecting app when it
                // differs from the stale pid. Keeps RPC authorization
                // (`SpawnWorkerPane`, BossOnly/AppOrBoss tiers) following
                // the live app across restarts. Only when a real trust
                // root was configured — test mode (`None`) stays
                // permissive so unit tests aren't pinned to a live pid.
                if let (Some(prior), Some(observed)) = (current_app_pid, peer_pid) {
                    if prior != observed {
                        server_state.set_app_pid(observed);
                        tracing::info!(
                            prior_app_pid = prior,
                            new_app_pid = observed,
                            "app session re-attached: trust root re-pinned to relaunched app",
                        );
                    }
                }
                server_state
                    .register_app_session(session_id.clone(), sink.clone())
                    .await;
                tracing::info!(session_id = %session_id, "app session registered");
                send_response(&sink, &request_id, FrontendEvent::AppSessionRegistered);
            }
            FrontendRequest::RegisterBossSession { shell_pid } => {
                // Only the registered app session may install the
                // Boss trust root.
                let app_session_id = server_state
                    .app_session
                    .lock()
                    .await
                    .as_ref()
                    .map(|h| h.session_id.clone());
                if app_session_id.as_deref() != Some(session_id.as_str()) {
                    tracing::warn!(
                        session_id = %session_id,
                        "register_boss_session rejected: caller is not the app session",
                    );
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::Error {
                            message: "register_boss_session: only the app session may install the Boss trust root"
                                .to_owned(),
                        },
                    );
                    continue;
                }
                server_state.set_boss_pid(shell_pid as libc::pid_t);
                tracing::info!(
                    boss_pid = shell_pid,
                    "boss session registered as second trust root",
                );
                send_response(&sink, &request_id, FrontendEvent::BossSessionRegistered);
            }
            FrontendRequest::EngineResponse {
                request_id: response_request_id,
                response,
            } => {
                server_state
                    .deliver_app_response(&session_id, &response_request_id, response)
                    .await;
            }
            FrontendRequest::ProbeRun { run_id, text, urgent } => {
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
                    continue;
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
                    FrontendEvent::ProbeQueued { run_id, probe_id, urgent },
                );
            }
            FrontendRequest::StopRun { run_id } => {
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
                    continue;
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
            FrontendRequest::FocusWorkerPane { run_id } => {
                // `bossctl agents focus` is a coordinator verb that
                // raises a sibling worker pane to the front. The
                // human invokes it from wherever they are — boss
                // pane, app shell, or another worker pane — so the
                // tier is `AppOrBoss`, matching `probe_run` /
                // `stop_run` (which are also legal from inside a
                // worker pane).
                if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
                    tracing::warn!(
                        peer_pid = ?peer_pid,
                        run_id = %run_id,
                        "focus_worker_pane rejected: caller not in app/Boss subtree",
                    );
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::Error {
                            message: "focus_worker_pane requires app or Boss authority".to_owned(),
                        },
                    );
                    continue;
                }
                match server_state.focus_worker_pane(&run_id).await {
                    Ok(slot_id) => {
                        tracing::info!(
                            run_id = %run_id,
                            slot_id,
                            "focus_worker_pane: pane raised",
                        );
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkerPaneFocused { run_id, slot_id },
                        );
                    }
                    Err(err) => {
                        tracing::warn!(?err, run_id = %run_id, "focus_worker_pane failed");
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: format!("focus_worker_pane: {err}"),
                            },
                        );
                    }
                }
            }
            FrontendRequest::SendInputToWorker { run_id, text } => {
                // `bossctl agents send` writes user-typed input into a
                // sibling worker pane. Same authority story as
                // `focus_worker_pane` / `probe_run` / `stop_run`: the
                // human invokes this from wherever they are (boss
                // pane, app shell, or another worker pane), so the
                // tier is `AppOrBoss` — caller must descend from the
                // app or the Boss session.
                if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
                    tracing::warn!(
                        peer_pid = ?peer_pid,
                        run_id = %run_id,
                        "send_input_to_worker rejected: caller not in app/Boss subtree",
                    );
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::Error {
                            message: "send_input_to_worker requires app or Boss authority"
                                .to_owned(),
                        },
                    );
                    continue;
                }
                match server_state.send_input_to_worker(&run_id, text).await {
                    Ok(slot_id) => {
                        tracing::info!(
                            run_id = %run_id,
                            slot_id,
                            "send_input_to_worker: text injected",
                        );
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkerInputSent { run_id, slot_id },
                        );
                    }
                    Err(err) => {
                        tracing::warn!(?err, run_id = %run_id, "send_input_to_worker failed");
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: format!("send_input_to_worker: {err}"),
                            },
                        );
                    }
                }
            }
            FrontendRequest::InterruptWorkerPane { run_id } => {
                // `bossctl agents interrupt` mirrors the keyboard Esc
                // a human would press inside the worker pane. Same
                // tier rationale as `focus_worker_pane`: the human
                // may invoke it from the Boss pane, the app shell,
                // or a sibling worker pane — `AppOrBoss` admits all
                // three.
                if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
                    tracing::warn!(
                        peer_pid = ?peer_pid,
                        run_id = %run_id,
                        "interrupt_worker_pane rejected: caller not in app/Boss subtree",
                    );
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::Error {
                            message: "interrupt_worker_pane requires app or Boss authority"
                                .to_owned(),
                        },
                    );
                    continue;
                }
                match server_state.interrupt_worker_pane(&run_id).await {
                    Ok(slot_id) => {
                        tracing::info!(
                            run_id = %run_id,
                            slot_id,
                            "interrupt_worker_pane: esc delivered",
                        );
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkerPaneInterrupted { run_id, slot_id },
                        );
                    }
                    Err(err) => {
                        tracing::warn!(?err, run_id = %run_id, "interrupt_worker_pane failed");
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: format!("interrupt_worker_pane: {err}"),
                            },
                        );
                    }
                }
            }
            FrontendRequest::RevealWorkItem { id } => {
                // `bossctl reveal` is a coordinator verb for navigating
                // the macOS app to a specific work item's card. Same
                // authority tier as `focus_worker_pane` — it's a UI
                // steering RPC invoked from the Boss pane or app shell.
                if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
                    tracing::warn!(
                        peer_pid = ?peer_pid,
                        id = %id,
                        "reveal_work_item rejected: caller not in app/Boss subtree",
                    );
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::Error {
                            message: "reveal_work_item requires app or Boss authority".to_owned(),
                        },
                    );
                    continue;
                }
                match server_state.reveal_work_item(&id).await {
                    Ok(canonical_id) => {
                        tracing::info!(
                            id = %id,
                            canonical_id = %canonical_id,
                            "reveal_work_item: card highlighted",
                        );
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkItemRevealed { id: canonical_id },
                        );
                    }
                    Err(err) => {
                        tracing::warn!(?err, id = %id, "reveal_work_item failed");
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: format!("reveal_work_item: {err}"),
                            },
                        );
                    }
                }
            }
            FrontendRequest::ListWorkerLiveStates => {
                let states = server_state.live_worker_states_snapshot();
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkerLiveStatesList { states },
                );
            }
            FrontendRequest::Shutdown { token } => {
                // The token written to disk at startup is the auth
                // credential — there is no pid-based tier check on
                // purpose. The whole point of the token gate (issue
                // #705) is that "same user / same machine" doesn't
                // separate the legitimate caller (macOS app, boss CLI)
                // from the accidental caller (a `bazel test` that
                // resolved the production socket). The bazel sandbox
                // already denies access to `~/Library/Application
                // Support/`, so a test that lands here without the
                // file in scope will fail with `token_missing` rather
                // than killing a 9-hour-old engine.
                let outcome = match server_state.control_token.as_deref() {
                    None => {
                        // In-process serve() without a control token —
                        // shouldn't happen for any process that has a
                        // dialable frontend socket, but the dispatcher
                        // is the wrong place to assume that. Reject
                        // explicitly rather than panic.
                        "token_missing"
                    }
                    Some(expected) => {
                        if constant_time_eq(expected.as_bytes(), token.as_bytes()) {
                            "accepted"
                        } else {
                            "token_mismatch"
                        }
                    }
                };
                crate::audit::record_shutdown_rpc(outcome, peer_pid.map(|p| p as i32));
                if outcome == "accepted" {
                    tracing::info!(
                        peer_pid = ?peer_pid,
                        "shutdown rpc: token accepted — graceful exit pending",
                    );
                    send_response(&sink, &request_id, FrontendEvent::ShutdownAccepted);
                    // Defer the actual notify so the writer task has a
                    // chance to drain the ShutdownAccepted frame into
                    // the kernel socket buffer before the accept loop
                    // breaks. 50 ms is well under the shutdown_workers
                    // grace window and well over the time it takes the
                    // dispatcher to enqueue + the writer task to flush.
                    let trigger = server_state.shutdown_trigger.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        trigger.notify_one();
                    });
                } else {
                    tracing::warn!(
                        peer_pid = ?peer_pid,
                        outcome,
                        "shutdown rpc: rejected",
                    );
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::ShutdownRejected {
                            reason: outcome.to_owned(),
                        },
                    );
                }
            }
            FrontendRequest::CancelExecution { execution_id } => {
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
                    continue;
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
            FrontendRequest::ReapRun { run_id } => {
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
                    continue;
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
            FrontendRequest::TailRunTranscript { run_id, lines } => {
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
                            message: "tail_run_transcript requires app or Boss authority"
                                .to_owned(),
                        },
                    );
                    continue;
                }
                match resolve_transcript_for_tail(&server_state, &run_id) {
                    TranscriptResolution::Found { transcript_path } => {
                        match read_transcript_tail(&transcript_path, lines).await {
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
                            Err(err) => {
                                send_response(
                                    &sink,
                                    &request_id,
                                    FrontendEvent::WorkError {
                                        message: format!(
                                            "transcript read failed for {transcript_path}: {err}"
                                        ),
                                    },
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
            FrontendRequest::WorkspacePoolSummary => {
                // Read-only view of `cube workspace list` plus engine
                // annotations. The coordinator contract documents this
                // as a bossctl verb, and any user who can run `cube
                // workspace list` directly already has the same view
                // — so an extra subtree gate buys no security and just
                // breaks legitimate calls (the live coordinator
                // session repro: bossctl invoked from a shell that's
                // neither an app nor a Boss descendant fell through
                // AppOrBoss). User tier is the right level.
                if !server_state.authorize_rpc(RpcTier::User, peer_pid) {
                    tracing::warn!(
                        peer_pid = ?peer_pid,
                        "workspace_pool_summary rejected: caller failed user tier",
                    );
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::Error {
                            message: "workspace_pool_summary failed user-tier check".to_owned(),
                        },
                    );
                    continue;
                }
                match server_state.cube_client.list_workspaces().await {
                    Ok(rows) => {
                        // Annotate each entry with the engine's view: which
                        // execution row (if any) currently records this
                        // workspace's lease. Drift (cube reports a lease the
                        // engine has no execution for) shows as `None`.
                        let lease_to_execution = match server_state.work_db.lease_to_execution_map()
                        {
                            Ok(map) => map,
                            Err(err) => {
                                tracing::warn!(
                                    ?err,
                                    "workspace_pool_summary: lease lookup failed; emitting cube view only",
                                );
                                std::collections::HashMap::new()
                            }
                        };
                        let workspaces =
                            rows.into_iter()
                                .map(|w| {
                                    let execution_id = w.lease_id.as_ref().and_then(|lease_id| {
                                        lease_to_execution.get(lease_id).cloned()
                                    });
                                    crate::protocol::WorkspacePoolEntry {
                                        workspace_id: w.workspace_id,
                                        workspace_path: w.workspace_path.display().to_string(),
                                        state: w.state,
                                        lease_id: w.lease_id,
                                        holder: w.holder,
                                        task: w.task,
                                        leased_at_epoch_s: w.leased_at_epoch_s,
                                        lease_expires_at_epoch_s: w.lease_expires_at_epoch_s,
                                        execution_id,
                                    }
                                })
                                .collect();
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkspacePoolSummaryResult { workspaces },
                        );
                    }
                    Err(err) => {
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: format!("cube workspace list failed: {err}"),
                            },
                        );
                    }
                }
            }
            FrontendRequest::AddDependency { input } => {
                match work_db.add_dependency(input) {
                    Ok(edge) => {
                        // Edge changes don't move any work item's status
                        // in this PR (status mechanics arrive in the
                        // follow-up phase), but we still publish a
                        // work-invalidation so subscribers re-render the
                        // dependency surfaces (kanban badge, show view).
                        let product_id = match work_db.get_work_item(&edge.dependent_id) {
                            Ok(item) => Some(work_item_product_id(&item)),
                            Err(_) => None,
                        };
                        let revision = if let Some(pid) = product_id.as_deref() {
                            publish_work_invalidation(
                                &server_state,
                                &session_id,
                                &request_id,
                                vec![work_product_topic(pid)],
                                "dependency_added",
                                Some(pid.to_owned()),
                                vec![edge.dependent_id.clone(), edge.prerequisite_id.clone()],
                            )
                            .await
                        } else {
                            server_state.current_work_revision()
                        };
                        send_response_with_revision(
                            &sink,
                            &request_id,
                            revision,
                            FrontendEvent::DependencyAdded { edge },
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
            FrontendRequest::RemoveDependency { input } => {
                let dependent_id = input.dependent.clone();
                let prerequisite_id = input.prerequisite.clone();
                let relation = input
                    .relation
                    .clone()
                    .unwrap_or_else(|| "blocks".to_owned());
                match work_db.remove_dependency(input) {
                    Ok(removed) => {
                        let product_id = match work_db.get_work_item(&dependent_id) {
                            Ok(item) => Some(work_item_product_id(&item)),
                            Err(_) => None,
                        };
                        let revision = if let Some(pid) = product_id.as_deref() {
                            publish_work_invalidation(
                                &server_state,
                                &session_id,
                                &request_id,
                                vec![work_product_topic(pid)],
                                "dependency_removed",
                                Some(pid.to_owned()),
                                vec![dependent_id.clone(), prerequisite_id.clone()],
                            )
                            .await
                        } else {
                            server_state.current_work_revision()
                        };
                        send_response_with_revision(
                            &sink,
                            &request_id,
                            revision,
                            FrontendEvent::DependencyRemoved {
                                dependent_id,
                                prerequisite_id,
                                relation,
                                removed,
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
            FrontendRequest::ListDependencies { input } => match work_db.list_dependencies(input) {
                Ok(view) => {
                    send_response(&sink, &request_id, FrontendEvent::DependencyList { view })
                }
                Err(err) => send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkError {
                        message: err.to_string(),
                    },
                ),
            },
            FrontendRequest::ListDependenciesDetailed { input } => {
                match work_db.list_dependencies_detailed(input) {
                    Ok(detail) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::DependencyDetail { detail },
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
            FrontendRequest::SetLiveStatusEnabled { slot_id, enabled } => {
                server_state
                    .live_status_manager
                    .set_enabled(slot_id, enabled);
                if let Err(err) = persist_live_status_disabled_slots(
                    &work_db,
                    &server_state.live_status_manager.disabled_snapshot(),
                ) {
                    tracing::warn!(
                        slot_id,
                        enabled,
                        ?err,
                        "live_status: failed to persist disabled-slot toggle",
                    );
                }
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::LiveStatusEnabledSet { slot_id, enabled },
                );
            }
            FrontendRequest::ListLiveStatusDisabledSlots => {
                let slot_ids = server_state.live_status_manager.disabled_snapshot();
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::LiveStatusDisabledSlotsList { slot_ids },
                );
            }
            FrontendRequest::DebugLiveStatusPipeline => {
                let report = build_live_status_debug_report(&server_state, &work_db);
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::LiveStatusDebugReportEvent { report },
                );
            }
            FrontendRequest::GetEngineVersion => {
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::EngineVersionResult {
                        git_sha: crate::build_info::git_sha().to_owned(),
                        build_time: crate::build_info::build_time().to_owned(),
                        binary_fingerprint: crate::build_info::binary_fingerprint().to_owned(),
                    },
                );
            }
            FrontendRequest::GetEngineHealth => {
                let report = build_engine_health_report(&server_state);
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::EngineHealthResult { report },
                );
            }
            FrontendRequest::ListFeatureFlags => {
                let flags = server_state
                    .feature_flags
                    .snapshot_all()
                    .into_iter()
                    .map(|snap| boss_protocol::FeatureFlagSnapshot {
                        name: snap.name,
                        description: snap.description,
                        category: snap.category,
                        default_enabled: snap.default_enabled,
                        enabled: snap.enabled,
                    })
                    .collect();
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::FeatureFlagsList { flags },
                );
            }
            FrontendRequest::SetFeatureFlag { name, enabled } => {
                match server_state.feature_flags.set(&name, enabled) {
                    Ok(()) => {
                        tracing::info!(
                            flag = %name,
                            enabled,
                            "feature-flags: toggled via macOS debug pane",
                        );
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::FeatureFlagSet { name, enabled },
                        );
                    }
                    Err(err) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: err.to_string(),
                        },
                    ),
                }
            }
            FrontendRequest::GetSettings => {
                let settings = server_state
                    .settings
                    .snapshot_all()
                    .into_iter()
                    .map(|snap| boss_protocol::SettingSnapshot {
                        key: snap.key,
                        description: snap.description,
                        default_enabled: snap.default_enabled,
                        enabled: snap.enabled,
                    })
                    .collect();
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::SettingsList { settings },
                );
            }
            FrontendRequest::SetSetting { key, enabled } => {
                match server_state.settings.set(&key, enabled) {
                    Ok(()) => {
                        tracing::info!(
                            %key,
                            enabled,
                            "settings: toggled via macOS Settings window",
                        );
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::SettingSet { key, enabled },
                        );
                    }
                    Err(err) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: err.to_string(),
                        },
                    ),
                }
            }
            FrontendRequest::MetricsShowLive { name } => {
                let counter = server_state.metrics.counter_snapshot_one(&name);
                let gauge = server_state.metrics.gauge_snapshot_one(&name);
                let entry = if let Some(snap) = counter {
                    Some(boss_protocol::MetricLiveEntry {
                        name: snap.name,
                        description: snap.description,
                        kind: "counter".into(),
                        value: snap.value as i64,
                        timestamp_ms: snap.updated_at_ms,
                        stale: snap.stale,
                    })
                } else {
                    gauge.map(|snap| boss_protocol::MetricLiveEntry {
                        name: snap.name,
                        description: snap.description,
                        kind: "gauge".into(),
                        value: snap.value,
                        timestamp_ms: snap.observed_at_ms,
                        stale: snap.stale,
                    })
                };
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::MetricsShowLiveResult { entry },
                );
            }
            FrontendRequest::MetricsListLive => {
                let mut entries: Vec<boss_protocol::MetricLiveEntry> = Vec::new();
                for snap in server_state.metrics.counter_snapshots() {
                    entries.push(boss_protocol::MetricLiveEntry {
                        name: snap.name,
                        description: snap.description,
                        kind: "counter".into(),
                        value: snap.value as i64,
                        timestamp_ms: snap.updated_at_ms,
                        stale: snap.stale,
                    });
                }
                for snap in server_state.metrics.gauge_snapshots() {
                    entries.push(boss_protocol::MetricLiveEntry {
                        name: snap.name,
                        description: snap.description,
                        kind: "gauge".into(),
                        value: snap.value,
                        timestamp_ms: snap.observed_at_ms,
                        stale: snap.stale,
                    });
                }
                entries.sort_by(|a, b| a.name.cmp(&b.name));
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::MetricsListLiveResult { entries },
                );
            }
            FrontendRequest::MetricsReset { name } => {
                let now = crate::metrics::registry::now_ms();
                let (counters_reset, gauges_reset) = match &name {
                    Some(n) => {
                        let (c, g) = server_state.metrics.reset_one(n);
                        if let Err(err) = work_db.metrics_reset_one(n, now) {
                            tracing::warn!(?err, metric = %n, "metrics reset: db update failed");
                        }
                        (c as u64, g as u64)
                    }
                    None => {
                        let (c, g) = server_state.metrics.reset_all();
                        if let Err(err) = work_db.metrics_reset_all(now) {
                            tracing::warn!(?err, "metrics reset --all: db update failed");
                        }
                        (c, g)
                    }
                };
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::MetricsResetDone { name, counters_reset, gauges_reset },
                );
            }
            FrontendRequest::KickPrReconcilers => {
                server_state.pr_reconciler_kick.notify_one();
                tracing::debug!("merge poller: activation kick received from app");
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::PrReconcilersKicked { kicked: true },
                );
            }
            FrontendRequest::CreateInvestigation { input } => {
                match work_db.create_investigation(input) {
                    Ok(task) => {
                        let item = WorkItem::Task(task);
                        let product_id = work_item_product_id(&item);
                        let revision = publish_work_invalidation(
                            &server_state,
                            &session_id,
                            &request_id,
                            vec![work_product_topic(&product_id)],
                            "investigation_created",
                            Some(product_id),
                            vec![work_item_id(&item)],
                        )
                        .await;
                        send_response_with_revision(
                            &sink,
                            &request_id,
                            revision,
                            FrontendEvent::WorkItemCreated { item },
                        );
                    }
                    Err(err) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: err.to_string(),
                        },
                    ),
                }
            }
            FrontendRequest::CreateRevision { input } => {
                match work_db.create_revision(input, &GhPrStateChecker) {
                    Ok(task) => {
                        let item = WorkItem::Task(task);
                        let product_id = work_item_product_id(&item);
                        let revision = publish_work_invalidation(
                            &server_state,
                            &session_id,
                            &request_id,
                            vec![work_product_topic(&product_id)],
                            "revision_created",
                            Some(product_id),
                            vec![work_item_id(&item)],
                        )
                        .await;
                        send_response_with_revision(
                            &sink,
                            &request_id,
                            revision,
                            FrontendEvent::WorkItemCreated { item },
                        );
                    }
                    Err(err) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: err.to_string(),
                        },
                    ),
                }
            }
            FrontendRequest::SetTaskInvestigationDoc { input } => {
                match work_db.set_task_investigation_doc(input) {
                    Ok(task) => {
                        let item = WorkItem::Task(task);
                        let product_id = work_item_product_id(&item);
                        let revision = publish_work_invalidation(
                            &server_state,
                            &session_id,
                            &request_id,
                            vec![work_product_topic(&product_id)],
                            "task_investigation_doc_set",
                            Some(product_id),
                            vec![work_item_id(&item)],
                        )
                        .await;
                        send_response_with_revision(
                            &sink,
                            &request_id,
                            revision,
                            FrontendEvent::WorkItemUpdated { item },
                        );
                    }
                    Err(err) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: err.to_string(),
                        },
                    ),
                }
            }
            FrontendRequest::SetProjectDesignDoc { input } => {
                match work_db.set_project_design_doc(input) {
                    Ok(project) => {
                        let item = WorkItem::Project(project);
                        let product_id = work_item_product_id(&item);
                        let revision = publish_work_invalidation(
                            &server_state,
                            &session_id,
                            &request_id,
                            vec![work_product_topic(&product_id)],
                            "project_design_doc_set",
                            Some(product_id),
                            vec![work_item_id(&item)],
                        )
                        .await;
                        send_response_with_revision(
                            &sink,
                            &request_id,
                            revision,
                            FrontendEvent::WorkItemUpdated { item },
                        );
                    }
                    Err(err) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: err.to_string(),
                        },
                    ),
                }
            }
            FrontendRequest::ResolveProjectDesignDoc { project_id } => {
                // Build a (repo_remote_url -> workspace_path) lookup so the
                // resolver can hand the open dispatcher an absolute
                // workspace path for `$EDITOR` / renderer fast-path.
                // First-match wins when multiple workspaces lease the
                // same repo — any of them resolves the file equally well.
                let leased_repo_paths: HashMap<String, String> = work_db
                    .list_in_flight_executions()
                    .map(|execs| {
                        let mut map = HashMap::new();
                        for exec in execs {
                            if let Some(path) = exec.workspace_path
                                && !map.contains_key(&exec.repo_remote_url)
                            {
                                map.insert(exec.repo_remote_url, path);
                            }
                        }
                        map
                    })
                    .unwrap_or_default();
                match work_db
                    .resolve_project_design_doc(&project_id, |repo| {
                        leased_repo_paths.get(repo).cloned()
                    })
                {
                    Ok(output) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::ProjectDesignDocResolved { output },
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
            FrontendRequest::ListConflictResolutions {
                product_id,
                status,
                work_item_id,
                limit,
            } => {
                // Read-only listing surface for `boss engine conflicts
                // list`. No auth gate — the rows are diagnostic and the
                // caller can already read the SQLite file.
                match work_db.list_conflict_resolutions(
                    product_id.as_deref(),
                    &status,
                    work_item_id.as_deref(),
                    limit,
                ) {
                    Ok(attempts) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::ConflictResolutionsList { attempts },
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
            FrontendRequest::GetConflictResolution { attempt_id } => {
                match work_db.get_conflict_resolution(&attempt_id) {
                    Ok(Some(attempt)) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::ConflictResolution { attempt },
                    ),
                    Ok(None) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: format!(
                                "conflict resolution attempt {attempt_id:?} is unknown",
                            ),
                        },
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
            FrontendRequest::RetryConflictResolution { attempt_id } => {
                match work_db.retry_conflict_resolution(&attempt_id) {
                    Ok(Some(attempt)) => {
                        tracing::warn!(
                            attempt_id = %attempt.id,
                            work_item_id = %attempt.work_item_id,
                            pr_url = %attempt.pr_url,
                            "retry_conflict_resolution: attempt reset to pending",
                        );
                        // Mirror the freshly-pending start so the macOS
                        // app's activity feed shows the retry as a new
                        // attempt. The wire shape is identical to the
                        // detection-path's started event — the consumer
                        // doesn't need to distinguish.
                        server_state
                            .publisher
                            .publish_frontend_event_on_product(
                                &attempt.product_id,
                                FrontendEvent::ConflictResolutionStarted {
                                    product_id: attempt.product_id.clone(),
                                    work_item_id: attempt.work_item_id.clone(),
                                    attempt_id: attempt.id.clone(),
                                    pr_url: attempt.pr_url.clone(),
                                },
                            )
                            .await;
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::ConflictResolutionRetried { attempt },
                        );
                    }
                    Ok(None) => {
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: format!(
                                    "conflict resolution attempt {attempt_id:?} is unknown or not in a terminal-failure state (only failed/abandoned rows can be retried)",
                                ),
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
            FrontendRequest::AbandonConflictResolution { attempt_id, reason } => {
                match work_db.mark_conflict_resolution_abandoned(&attempt_id, &reason) {
                    Ok(Some(attempt)) => {
                        tracing::warn!(
                            attempt_id = %attempt.id,
                            work_item_id = %attempt.work_item_id,
                            pr_url = %attempt.pr_url,
                            %reason,
                            "abandon_conflict_resolution: attempt flipped to abandoned",
                        );
                        server_state
                            .publisher
                            .publish_frontend_event_on_product(
                                &attempt.product_id,
                                FrontendEvent::ConflictResolutionAbandoned {
                                    product_id: attempt.product_id.clone(),
                                    work_item_id: attempt.work_item_id.clone(),
                                    attempt_id: attempt.id.clone(),
                                    pr_url: attempt.pr_url.clone(),
                                    failure_reason: reason.clone(),
                                },
                            )
                            .await;
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::ConflictResolutionMarkedAbandoned { attempt },
                        );
                    }
                    Ok(None) => {
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: format!(
                                    "conflict resolution attempt {attempt_id:?} is unknown or already terminal",
                                ),
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
            FrontendRequest::MarkConflictResolutionFailed { attempt_id, reason } => {
                // Worker-facing stop-condition surface. `User` tier:
                // the worker pane invokes `boss engine conflicts
                // mark-failed`, which descends from a worker pane and
                // therefore wouldn't pass `AppOrBoss`. The only state
                // change is on a `conflict_resolutions` row keyed by
                // an opaque id — a worker forging an attempt id has
                // no row to clobber, so authority gates aren't
                // load-bearing here.
                match work_db.mark_conflict_resolution_failed(&attempt_id, &reason) {
                    Ok(Some(attempt)) => {
                        tracing::warn!(
                            attempt_id = %attempt.id,
                            work_item_id = %attempt.work_item_id,
                            pr_url = %attempt.pr_url,
                            %reason,
                            "mark_conflict_resolution_failed: attempt flipped to failed",
                        );
                        // Phase 4 #12: broadcast the typed activity-feed
                        // event so subscribers (the macOS app) can
                        // render the failed-attempt entry without
                        // round-tripping through the CLI's response.
                        server_state
                            .publisher
                            .publish_frontend_event_on_product(
                                &attempt.product_id,
                                FrontendEvent::ConflictResolutionFailed {
                                    product_id: attempt.product_id.clone(),
                                    work_item_id: attempt.work_item_id.clone(),
                                    attempt_id: attempt.id.clone(),
                                    pr_url: attempt.pr_url.clone(),
                                    failure_reason: reason.clone(),
                                },
                            )
                            .await;
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::ConflictResolutionMarkedFailed { attempt },
                        );
                    }
                    Ok(None) => {
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: format!(
                                    "conflict resolution attempt {attempt_id:?} is unknown or already terminal",
                                ),
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
            FrontendRequest::ClassifyCiRemediation {
                attempt_id,
                triage_class,
            } => {
                // Worker-facing marker: stamp `triage_class` on a
                // `ci_remediations` row. Pure metadata column, no
                // authority gate — a forged attempt id has no row to
                // clobber.
                match work_db.set_ci_remediation_triage_class(&attempt_id, &triage_class) {
                    Ok(Some(attempt)) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::CiRemediationClassified { attempt },
                    ),
                    Ok(None) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: format!(
                                "ci_remediation attempt {attempt_id:?} is unknown",
                            ),
                        },
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
            FrontendRequest::MarkCiRemediationFailed { attempt_id, reason } => {
                match work_db.mark_ci_remediation_failed(&attempt_id, &reason) {
                    Ok(Some(attempt)) => {
                        tracing::warn!(
                            attempt_id = %attempt.id,
                            work_item_id = %attempt.work_item_id,
                            pr_url = %attempt.pr_url,
                            %reason,
                            "mark_ci_remediation_failed: attempt flipped to failed",
                        );
                        server_state
                            .publisher
                            .publish_frontend_event_on_product(
                                &attempt.product_id,
                                FrontendEvent::CiRemediationFailed {
                                    product_id: attempt.product_id.clone(),
                                    work_item_id: attempt.work_item_id.clone(),
                                    attempt_id: attempt.id.clone(),
                                    pr_url: attempt.pr_url.clone(),
                                    failure_reason: reason.clone(),
                                },
                            )
                            .await;
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::CiRemediationMarkedFailed { attempt },
                        );
                    }
                    Ok(None) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: format!(
                                "ci_remediation attempt {attempt_id:?} is unknown or already terminal",
                            ),
                        },
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
            FrontendRequest::MarkCiRemediationRetriggered { attempt_id, new_id } => {
                // The retrigger marker doesn't change the row's status —
                // the merge-poller observes the re-run's outcome on the
                // next sweep. We just log + echo so the worker has a
                // confirmation receipt.
                match work_db.get_ci_remediation(&attempt_id) {
                    Ok(Some(attempt)) => {
                        tracing::info!(
                            attempt_id = %attempt.id,
                            work_item_id = %attempt.work_item_id,
                            new_id = %new_id,
                            "mark_ci_remediation_retriggered: worker re-ran the failing build",
                        );
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::CiRemediationRetriggered {
                                attempt,
                                new_id,
                            },
                        );
                    }
                    Ok(None) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: format!("ci_remediation attempt {attempt_id:?} is unknown"),
                        },
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
            FrontendRequest::MarkCiRemediationSucceededViaRebase { attempt_id } => {
                // Snapshot the pre-update row so we can report
                // `budget_refunded` accurately (only fix-kind attempts
                // with `consumes_budget = 1` get a counter decrement).
                let pre = work_db.get_ci_remediation(&attempt_id).ok().flatten();
                match work_db.mark_ci_remediation_succeeded_via_rebase(&attempt_id) {
                    Ok(Some(attempt)) => {
                        let budget_refunded = pre
                            .as_ref()
                            .map(|p| p.consumes_budget != 0)
                            .unwrap_or(false);
                        tracing::info!(
                            attempt_id = %attempt.id,
                            work_item_id = %attempt.work_item_id,
                            budget_refunded,
                            "mark_ci_remediation_succeeded_via_rebase: rebase-only success recorded",
                        );
                        server_state
                            .publisher
                            .publish_frontend_event_on_product(
                                &attempt.product_id,
                                FrontendEvent::CiRemediationSucceeded {
                                    product_id: attempt.product_id.clone(),
                                    work_item_id: attempt.work_item_id.clone(),
                                    attempt_id: attempt.id.clone(),
                                    pr_url: attempt.pr_url.clone(),
                                },
                            )
                            .await;
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::CiRemediationSucceededViaRebase {
                                attempt,
                                budget_refunded,
                            },
                        );
                    }
                    Ok(None) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: format!(
                                "ci_remediation attempt {attempt_id:?} is unknown or already terminal",
                            ),
                        },
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
            FrontendRequest::ListCiRemediations {
                product_id,
                status,
                work_item_id,
                limit,
            } => {
                // Read-only listing surface for `boss engine ci list`
                // (design Phase 11 #35). Mirror of
                // `ListConflictResolutions`.
                match work_db.list_ci_remediations(
                    product_id.as_deref(),
                    &status,
                    work_item_id.as_deref(),
                    limit,
                ) {
                    Ok(attempts) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::CiRemediationsList { attempts },
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
            FrontendRequest::GetCiRemediation { attempt_id } => {
                match work_db.get_ci_remediation(&attempt_id) {
                    Ok(Some(attempt)) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::CiRemediation { attempt },
                    ),
                    Ok(None) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: format!(
                                "ci_remediation attempt {attempt_id:?} is unknown",
                            ),
                        },
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
            FrontendRequest::RetryCiRemediation { selector } => {
                // The CLI accepts either a `ci_remediations` attempt id
                // or a work-item id (design Q11 "When invoked on an
                // attempt id, the engine resolves the attempt to its
                // work_item_id and acts on the parent."). Resolve the
                // selector before invoking the engine path so the
                // error messages stay grounded in what the caller
                // typed.
                let resolved: Result<Option<String>, anyhow::Error> = if selector
                    .starts_with("cir_")
                {
                    work_db
                        .get_ci_remediation(&selector)
                        .map(|opt| opt.map(|a| a.work_item_id))
                } else {
                    Ok(Some(selector.clone()))
                };
                match resolved {
                    Ok(Some(work_item_id)) => {
                        match work_db.retry_ci_remediation_for_work_item(&work_item_id) {
                            Ok(Some((budget, was_exhausted))) => {
                                tracing::warn!(
                                    %work_item_id,
                                    was_exhausted,
                                    "retry_ci_remediation: budget reset, parent unblocked={was_exhausted}",
                                );
                                send_response(
                                    &sink,
                                    &request_id,
                                    FrontendEvent::CiRemediationRetryDone {
                                        work_item_id,
                                        budget,
                                        was_exhausted,
                                    },
                                );
                            }
                            Ok(None) => send_response(
                                &sink,
                                &request_id,
                                FrontendEvent::WorkError {
                                    message: format!(
                                        "work item {work_item_id:?} is unknown",
                                    ),
                                },
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
                    Ok(None) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: format!(
                                "ci_remediation attempt {selector:?} is unknown",
                            ),
                        },
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
            FrontendRequest::AbandonCiRemediation { attempt_id, reason } => {
                match work_db.mark_ci_remediation_abandoned(&attempt_id, &reason) {
                    Ok(Some(attempt)) => {
                        tracing::warn!(
                            attempt_id = %attempt.id,
                            work_item_id = %attempt.work_item_id,
                            pr_url = %attempt.pr_url,
                            %reason,
                            "abandon_ci_remediation: attempt flipped to abandoned",
                        );
                        server_state
                            .publisher
                            .publish_frontend_event_on_product(
                                &attempt.product_id,
                                FrontendEvent::CiRemediationAbandoned {
                                    product_id: attempt.product_id.clone(),
                                    work_item_id: attempt.work_item_id.clone(),
                                    attempt_id: attempt.id.clone(),
                                    pr_url: attempt.pr_url.clone(),
                                    failure_reason: reason.clone(),
                                },
                            )
                            .await;
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::CiRemediationMarkedAbandoned { attempt },
                        );
                    }
                    Ok(None) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: format!(
                                "ci_remediation attempt {attempt_id:?} is unknown or already terminal",
                            ),
                        },
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
            FrontendRequest::GetCiBudget { work_item_id } => {
                match work_db.ci_budget_snapshot(&work_item_id) {
                    Ok(Some(budget)) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::CiBudget { budget },
                    ),
                    Ok(None) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: format!("work item {work_item_id:?} is unknown"),
                        },
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
            FrontendRequest::SetCiBudget {
                work_item_id,
                budget,
            } => {
                match work_db.set_ci_attempt_budget(&work_item_id, budget) {
                    Ok(Some(snapshot)) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::CiBudgetUpdated { budget: snapshot },
                    ),
                    Ok(None) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: format!("work item {work_item_id:?} is unknown"),
                        },
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
            FrontendRequest::ListEngineAttempts {
                kinds,
                product_id,
                status,
                work_item_id,
                limit,
            } => {
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
            FrontendRequest::SetProductDefaultModel { product_id, model } => {
                match work_db.set_product_default_model(&product_id, model.as_deref()) {
                    Ok(product) => {
                        let item = WorkItem::Product(product);
                        let pid = work_item_product_id(&item);
                        let revision = publish_work_invalidation(
                            &server_state,
                            &session_id,
                            &request_id,
                            vec![work_product_topic(&pid)],
                            "product_default_model_set",
                            Some(pid),
                            vec![work_item_id(&item)],
                        )
                        .await;
                        send_response_with_revision(
                            &sink,
                            &request_id,
                            revision,
                            FrontendEvent::WorkItemUpdated { item },
                        );
                    }
                    Err(err) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: err.to_string(),
                        },
                    ),
                }
            }
            FrontendRequest::AuditProductEffort {
                product_id,
                window_days,
            } => {
                // Read-only diagnostic surface for `boss product
                // audit-effort`. No auth gate — the rows are the
                // chore corpus the caller can already enumerate
                // via `boss chore list`, and the escalation events
                // are coordinator-emitted facts about that corpus.
                let result = build_effort_audit_report(
                    &work_db,
                    &product_id,
                    window_days,
                );
                match result {
                    Ok(report) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::EffortAuditReport { report },
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
            FrontendRequest::RecordEffortEscalation {
                work_item_id,
                original_level,
                new_level,
                markers,
                rule_id,
            } => {
                // Coordinator-only RPC in practice (the sibling
                // escalation-handler task is the only caller in
                // v1), but the engine doesn't gate it — the row
                // is opaque diagnostic data and a forged event is
                // bounded to one false-positive in the audit
                // report.
                match work_db.record_effort_escalation(
                    &work_item_id,
                    original_level,
                    new_level,
                    &markers,
                    rule_id.as_deref(),
                ) {
                    Ok(event) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::EffortEscalationRecorded { event },
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
            FrontendRequest::SetProductExternalTracker { input } => {
                let validation_result = if input.unset {
                    Ok(())
                } else {
                    match (input.kind.as_deref(), input.config.as_ref()) {
                        (None, _) | (_, None) => Err("both kind and config must be provided when not using unset".to_owned()),
                        (Some(kind), Some(config)) => validate_external_tracker_config(kind, config),
                    }
                };
                match validation_result {
                    Err(msg) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError { message: msg },
                    ),
                    Ok(()) => {
                        let result = work_db.set_product_external_tracker(
                            &input.product_id,
                            input.kind.as_deref(),
                            input.config.as_ref(),
                            input.unset,
                        );
                        match result {
                            Ok(product) => {
                                let item = WorkItem::Product(product);
                                let product_id = work_item_product_id(&item);
                                let revision = publish_work_invalidation(
                                    &server_state,
                                    &session_id,
                                    &request_id,
                                    vec![work_product_topic(&product_id)],
                                    "external_tracker_updated",
                                    Some(product_id),
                                    vec![work_item_id(&item)],
                                )
                                .await;
                                send_response_with_revision(
                                    &sink,
                                    &request_id,
                                    revision,
                                    FrontendEvent::WorkItemUpdated { item },
                                );
                            }
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
            }
            FrontendRequest::SyncProductExternalTracker { product_id } => {
                let work_db = server_state.work_db.clone();
                let registry = server_state.tracker_registry.clone();
                let metrics = server_state.metrics.clone();
                let publisher = server_state.clone();
                let sink2 = sink.clone();
                let request_id2 = request_id.clone();
                tokio::spawn(async move {
                    match crate::external_tracker::reconcile::run_one_pass_for_product(
                        work_db.as_ref(),
                        registry.as_ref(),
                        metrics.as_ref(),
                        &product_id,
                        publisher.as_ref(),
                    )
                    .await
                    {
                        Some(outcome) => {
                            tracing::info!(
                                product_id,
                                items_imported = outcome.items_imported,
                                items_closed = outcome.items_closed,
                                pr_attached = outcome.pr_attached,
                                close_issue_succeeded = outcome.close_issue_succeeded,
                                close_issue_failed = outcome.close_issue_failed,
                                items_unbound = outcome.items_unbound,
                                "on-demand external tracker sync complete",
                            );
                            send_response(
                                &sink2,
                                &request_id2,
                                FrontendEvent::ExternalTrackerSyncStarted { product_id },
                            );
                        }
                        None => {
                            send_response(
                                &sink2,
                                &request_id2,
                                FrontendEvent::WorkError {
                                    message: format!(
                                        "product '{product_id}' has no external tracker binding"
                                    ),
                                },
                            );
                        }
                    }
                });
            }
            FrontendRequest::LinkWorkItemExternalRef { input } => {
                let result = work_db
                    .set_external_ref(
                        &input.work_item_id,
                        &input.kind,
                        &input.canonical_id,
                        &serde_json::Value::Null,
                    )
                    .and_then(|()| work_db.get_task_with_external_ref(&input.work_item_id));
                match result {
                    Ok(item) => {
                        let product_id = work_item_product_id(&item);
                        let revision = publish_work_invalidation(
                            &server_state,
                            &session_id,
                            &request_id,
                            vec![work_product_topic(&product_id)],
                            "work_item_updated",
                            Some(product_id),
                            vec![work_item_id(&item)],
                        )
                        .await;
                        send_response_with_revision(
                            &sink,
                            &request_id,
                            revision,
                            FrontendEvent::WorkItemUpdated { item },
                        );
                    }
                    Err(err) => send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: err.to_string(),
                        },
                    ),
                }
            }
            FrontendRequest::UnlinkWorkItemExternalRef { work_item_id: target_id } => {
                let result = work_db
                    .clear_external_ref(&target_id)
                    .and_then(|()| work_db.get_task_with_external_ref(&target_id));
                match result {
                    Ok(item) => {
                        let product_id = work_item_product_id(&item);
                        let revision = publish_work_invalidation(
                            &server_state,
                            &session_id,
                            &request_id,
                            vec![work_product_topic(&product_id)],
                            "work_item_updated",
                            Some(product_id),
                            vec![work_item_id(&item)],
                        )
                        .await;
                        send_response_with_revision(
                            &sink,
                            &request_id,
                            revision,
                            FrontendEvent::WorkItemUpdated { item },
                        );
                    }
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
    }

    server_state.topic_broker.remove_session(&session_id).await;
    server_state.drop_app_session_if_matches(&session_id).await;
    sink.close();
    let _ = writer_task.await;
    Ok(())
}

/// Build the per-product effort-audit report. Handles the product
/// lookup, window filter, and chore-corpus / event-log fan-in so
/// the RPC handler stays a thin error-translation layer.
fn build_effort_audit_report(
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
    let events =
        work_db.list_effort_escalations_for_product(&product.id, since_epoch_secs)?;
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

/// Persist the disabled-slot snapshot to the metadata KV. Called
/// from the toggle handler. Errors bubble up so the caller can log
/// them — persistence failure is non-fatal (the in-memory set still
/// honours the toggle until restart).
fn persist_live_status_disabled_slots(work_db: &WorkDb, slot_ids: &[u8]) -> Result<()> {
    let joined = slot_ids
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");
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
fn build_engine_health_report(server_state: &Arc<ServerState>) -> boss_protocol::EngineHealthReport {
    use boss_protocol::{EngineHealthIssue, EngineHealthReport};

    let anthropic_api_key_present = server_state.anthropic_api_key.is_some();
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

    EngineHealthReport {
        anthropic_api_key_present,
        issues,
    }
}

/// Build the per-slot diagnostic snapshot the `live-status debug`
/// verb returns. Reads the manager's debug store, joins with the
/// per-slot live state (for transcript_path lookup via WorkDb), and
/// stamps engine-level facts (build SHA, API key presence). No
/// blocking IO is acceptable here — this verb is called interactively
/// and must return promptly even when the engine is busy.
fn build_live_status_debug_report(
    server_state: &Arc<ServerState>,
    work_db: &WorkDb,
) -> boss_protocol::LiveStatusDebugReport {
    use boss_protocol::{LiveStatusDebugReport, LiveStatusSlotDebug};
    let manager = &server_state.live_status_manager;
    let live_states = server_state.live_worker_states.snapshot();
    let store = manager.debug_store();
    let store_snapshots = store.snapshot_all();
    let active_slots = manager.active_slot_ids();
    let disabled_set: std::collections::HashSet<u8> =
        manager.disabled_snapshot().into_iter().collect();

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
        let transcript_path = snap
            .transcript_path
            .clone()
            .or_else(|| {
                let execution_id = live.map(|s| s.run_id.as_str())?;
                work_db
                    .transcript_path_for_execution(execution_id)
                    .ok()
                    .flatten()
            });
        slots.push(LiveStatusSlotDebug {
            slot_id,
            task_running: active_slots.contains(&slot_id),
            disabled: disabled_set.contains(&slot_id),
            last_trigger_kind: snap.last_trigger_kind.clone(),
            last_trigger_at: snap.last_trigger_at_epoch_s.map(format_epoch_iso8601),
            last_real_trigger_kind: snap.last_real_trigger_kind.clone(),
            last_real_trigger_at: snap
                .last_real_trigger_at_epoch_s
                .map(format_epoch_iso8601),
            last_synthetic_trigger_at: snap
                .last_synthetic_trigger_at_epoch_s
                .map(format_epoch_iso8601),
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
        hook_events_with_transcript_path_in_payload: stats
            .hook_events_with_transcript_path_in_payload,
        hook_events_without_transcript_path_in_payload: stats
            .hook_events_without_transcript_path_in_payload,
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

fn format_epoch_iso8601(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let seconds_in_day = epoch_secs.rem_euclid(86_400);
    let hour = seconds_in_day / 3_600;
    let minute = (seconds_in_day % 3_600) / 60;
    let second = seconds_in_day % 60;
    let (year, month, day) = ymd_from_days_since_1970(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    )
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
fn load_live_status_disabled_slots(work_db: &WorkDb) -> Vec<u8> {
    let Ok(Some(raw)) = work_db.get_metadata(META_LIVE_STATUS_DISABLED_SLOTS) else {
        return Vec::new();
    };
    raw.split(',')
        .filter_map(|s| s.trim().parse::<u8>().ok())
        .collect()
}

/// Downcast `err` to `DuplicateTaskError` and return a structured
/// `WorkItemDuplicateBlocked` event; fall back to `WorkError` for any
/// other error kind. Keeps the `CreateTask` / `CreateChore` error arms
/// DRY.
fn duplicate_or_work_error(err: anyhow::Error) -> FrontendEvent {
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

fn send_response(sink: &SessionSink, request_id: &str, payload: FrontendEvent) {
    sink.enqueue(FrontendEventEnvelope::response(
        request_id.to_owned(),
        payload,
    ));
}

fn send_response_with_revision(
    sink: &SessionSink,
    request_id: &str,
    revision: u64,
    payload: FrontendEvent,
) {
    sink.enqueue(FrontendEventEnvelope::response_with_revision(
        request_id.to_owned(),
        revision,
        payload,
    ));
}

fn send_push(sink: &SessionSink, payload: FrontendEvent) {
    sink.enqueue(FrontendEventEnvelope::push(payload));
}

async fn publish_work_invalidation(
    server_state: &ServerState,
    origin_session_id: &str,
    origin_request_id: &str,
    topics: Vec<String>,
    reason: &str,
    product_id: Option<String>,
    item_ids: Vec<String>,
) -> u64 {
    if let Some(product_id) = product_id.as_deref() {
        match server_state
            .work_db
            .reconcile_product_executions(product_id)
        {
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
        if let FrontendEvent::TopicEvent {
            topic: event_topic, ..
        } = &mut event
        {
            *event_topic = topic.clone();
        }
        server_state
            .topic_broker
            .publish(
                &topic,
                FrontendEventEnvelope::push_with_revision(revision, event),
            )
            .await;
    }

    revision
}

/// Bulk counterpart of [`publish_work_invalidation`]. Emits one
/// `WorkInvalidated` topic event per distinct `product_id` carrying
/// only that product's item ids, all at the same fresh revision —
/// kanban consumers reload their product once. Returns the shared
/// revision so the caller can stamp it on the unicast response.
async fn publish_batch_work_invalidation(
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
        match server_state
            .work_db
            .reconcile_product_executions(product_id)
        {
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
            .publish(
                &topic,
                FrontendEventEnvelope::push_with_revision(revision, event),
            )
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
async fn handle_create_many(
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
            let revision = publish_batch_work_invalidation(
                server_state,
                session_id,
                request_id,
                reason,
                &items,
            )
            .await;
            send_response_with_revision(
                sink,
                request_id,
                revision,
                FrontendEvent::WorkItemsCreated { items },
            );
        }
        Err(err) => {
            send_response(sink, request_id, duplicate_or_work_error(err));
        }
    }
}

/// Transport-layer fallback for `created_via` when a caller didn't
/// stamp it themselves. The macOS app self-identifies via
/// `RegisterAppSession`, so any request from the registered app
/// session defaults to `mac_app`; everything else (CLI, bossctl,
/// ad-hoc test client) falls through to `unknown`. CLI / bossctl
/// always set the field explicitly, so `unknown` here only fires for
/// off-the-beaten-path callers — exactly the case we want to flag in
/// the database rather than mislabel.
async fn transport_default_created_via(
    server_state: &Arc<ServerState>,
    session_id: &str,
) -> String {
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

fn work_item_id(item: &WorkItem) -> String {
    match item {
        WorkItem::Product(product) => product.id.clone(),
        WorkItem::Project(project) => project.id.clone(),
        WorkItem::Task(task) | WorkItem::Chore(task) => task.id.clone(),
    }
}

/// Validate a kind-specific external tracker config JSON.
/// Returns `Err` with a human-readable message when validation fails.
fn validate_external_tracker_config(
    kind: &str,
    config: &serde_json::Value,
) -> Result<(), String> {
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
                None => return Err("missing required field 'project_number' for kind=github".to_owned()),
                Some(v) if !v.is_number() => return Err("'project_number' must be a number for kind=github".to_owned()),
                _ => {}
            }
            Ok(())
        }
        other => Err(format!("unknown tracker kind '{other}'; supported: github")),
    }
}

fn work_item_product_id(item: &WorkItem) -> String {
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
fn task_status_for_id(work_db: &WorkDb, id: &str) -> Option<String> {
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
fn work_item_needs_dispatch(work_db: &WorkDb, work_item_id: &str) -> bool {
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
fn task_transitioned_to_active(previous_status: &Option<String>, item: &WorkItem) -> bool {
    let task = match item {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        _ => return false,
    };
    if task.status != "active" {
        return false;
    }
    match previous_status {
        Some(prev) => prev != "active",
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
fn terminal_chore_execution(work_db: &WorkDb, item: &WorkItem) -> Option<String> {
    let task = match item {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        _ => return None,
    };
    if !matches!(task.status.as_str(), "done" | "archived" | "cancelled") {
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
fn in_review_chore_execution(work_db: &WorkDb, item: &WorkItem) -> Option<String> {
    let task = match item {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        _ => return None,
    };
    if task.status != "in_review" {
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

/// Return `(name, description)` for a task/chore id, or `None` when
/// the id does not name a task/chore or cannot be read from the DB.
/// Used by the `UpdateWorkItem` handler to snapshot the spec before an
/// edit so the chore-update worker notification can show old vs. new.
fn task_name_description_for_id(work_db: &WorkDb, id: &str) -> Option<(String, String)> {
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
fn active_chore_run_id(server_state: &ServerState, item: &WorkItem) -> Option<String> {
    let task = match item {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        _ => return None,
    };
    if task.status != "active" {
        return None;
    }
    server_state
        .live_worker_states
        .run_id_for_work_item(&task.id)
}

/// Build the `[chore-update]` notice text. Returns `None` when neither
/// name nor description actually changed (so the caller can skip the
/// send).
fn build_chore_update_message(
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
const TRANSCRIPT_NOT_YET_AVAILABLE_PREFIX: &str = "transcript not yet available for run ";

/// Outcome of [`resolve_transcript_for_tail`].
#[derive(Debug, PartialEq, Eq)]
enum TranscriptResolution {
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
fn resolve_transcript_for_tail(
    server_state: &ServerState,
    run_id: &str,
) -> TranscriptResolution {
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
    if let Some(transcript_path) = run_lookup
        .as_ref()
        .and_then(|run| run.transcript_path.clone())
    {
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
    let execution_known = server_state.work_db.get_execution(run_id).is_ok();
    let is_live = server_state.live_worker_states.is_run_live(run_id);

    if is_live {
        return TranscriptResolution::Buffering;
    }
    if run_known || execution_known {
        return TranscriptResolution::KnownNoTranscript;
    }
    TranscriptResolution::Unknown
}

async fn read_transcript_tail(
    transcript_path: &str,
    lines: usize,
) -> std::io::Result<(Vec<String>, bool)> {
    let contents = tokio::fs::read_to_string(transcript_path).await?;
    let split_lines: Vec<&str> = contents.lines().collect();
    if lines == 0 {
        return Ok((Vec::new(), !split_lines.is_empty()));
    }
    let total = split_lines.len();
    let take = lines.min(total);
    let truncated = total > take;
    let tail = split_lines
        .into_iter()
        .skip(total - take)
        .map(str::to_owned)
        .collect();
    Ok((tail, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::TopicEventPayload;

    fn topic_envelope(topic: &str, revision: u64) -> FrontendEventEnvelope {
        FrontendEventEnvelope::push_with_revision(
            revision,
            FrontendEvent::TopicEvent {
                topic: topic.to_owned(),
                revision,
                origin_session_id: "test".to_owned(),
                origin_request_id: None,
                event: TopicEventPayload::WorkInvalidated {
                    reason: "test".to_owned(),
                    product_id: None,
                    item_ids: vec![],
                },
            },
        )
    }

    fn response_envelope(request_id: &str) -> FrontendEventEnvelope {
        FrontendEventEnvelope::response(
            request_id.to_owned(),
            FrontendEvent::ProductsList { products: vec![] },
        )
    }

    fn topic_of(env: &FrontendEventEnvelope) -> Option<String> {
        topic_event_topic(&env.payload)
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

    fn test_server_state() -> Arc<ServerState> {
        let temp = tempfile::tempdir().unwrap();
        let cfg = Arc::new(RuntimeConfig::from_parts(
            crate::config::WorkConfig {
                cwd: temp.path().to_path_buf(),
                db_path: temp.path().join("state.db"),
                worker_pool_size: 1,
            },
            None,
        ));
        // Leak the temp dir for the lifetime of the test process; the
        // ServerState's WorkDb keeps a handle to a path inside it.
        std::mem::forget(temp);
        ServerState::new_arc_with_app_pid(cfg, None, None).unwrap()
    }

    fn make_session_sink() -> Arc<SessionSink> {
        let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
        Arc::new(SessionSink::new(shutdown_tx))
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
        let work = crate::config::WorkConfig {
            cwd: temp.path().to_path_buf(),
            db_path: temp.path().join("state.db"),
            worker_pool_size: 1,
        };
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
        let conn = tokio::spawn(handle_frontend_connection(
            engine_side,
            server_state,
            None,
        ));

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
            .send_input_to_worker("never-allocated", "/help\n".into())
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
                .send_input_to_worker("run-send", "/help\n".into())
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
                .send_input_to_worker("run-send", "hi\n".into())
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
        server_state
            .worker_registry
            .register_run_slot("run-int", 6);

        let sink = make_session_sink();
        server_state
            .register_app_session("session-app".into(), sink.clone())
            .await;

        let server_clone = server_state.clone();
        let interrupt = tokio::spawn(async move {
            server_clone.interrupt_worker_pane("run-int").await
        });

        let envelope = sink
            .next()
            .await
            .expect("an EngineRequest event should be enqueued");
        let (request_id, request) = match envelope.payload {
            FrontendEvent::EngineRequest { request_id, request } => (request_id, request),
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
        server_state
            .worker_registry
            .register_run_slot("run-int", 2);

        let sink = make_session_sink();
        server_state
            .register_app_session("session-app".into(), sink.clone())
            .await;

        let server_clone = server_state.clone();
        let interrupt = tokio::spawn(async move {
            server_clone.interrupt_worker_pane("run-int").await
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
            crate::config::WorkConfig {
                cwd: temp.path().to_path_buf(),
                db_path: temp.path().join("state.db"),
                worker_pool_size: 1,
            },
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
        assert!(pid_is_alive(self_pid), "the current process must read as alive");
        assert!(!pid_is_alive(0), "pid 0 must never read as a live trust root");
        assert!(!pid_is_alive(reaped_child_pid()), "a reaped child must read as dead");
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
        assert!(!register_app_session_trust_ok(Some(self_pid), None, engine_pid));
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
            .request_execution(RequestExecutionInput {
                work_item_id: chore.id.clone(),
                priority: None,
                preferred_workspace_id: None,
                force: false,
            })
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
        server_state.worker_registry.register_run_slot(execution.id.clone(), 1);

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
            .request_execution(RequestExecutionInput {
                work_item_id: chore.id.clone(),
                priority: None,
                preferred_workspace_id: None,
                force: false,
            })
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
        server_state
            .live_worker_states
            .register_spawn(1, run.id.clone(), "claude-opus-4-7", 0, None);
        // Apply a Stop event to transition Spawning → Idle.
        server_state
            .live_worker_states
            .apply_event(1, &WorkerEvent::Stop {
                session_id: "sess-1".into(),
                stop_hook_active: false,
                stop_reason: crate::protocol::StopReason::Completed,
            });
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
            .request_execution(RequestExecutionInput {
                work_item_id: chore.id.clone(),
                priority: None,
                preferred_workspace_id: None,
                force: false,
            })
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
            .request_execution(RequestExecutionInput {
                work_item_id: chore.id.clone(),
                priority: None,
                preferred_workspace_id: None,
                force: false,
            })
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
            .request_execution(RequestExecutionInput {
                work_item_id: chore.id.clone(),
                priority: None,
                preferred_workspace_id: None,
                force: false,
            })
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
            .request_execution(RequestExecutionInput {
                work_item_id: chore.id.clone(),
                priority: None,
                preferred_workspace_id: None,
                force: false,
            })
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
        use crate::protocol::{WorkerEvent, SessionStartSource};
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
            .request_execution(RequestExecutionInput {
                work_item_id: chore.id.clone(),
                priority: None,
                preferred_workspace_id: None,
                force: false,
            })
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
        use boss_protocol::{
            CreateChoreInput, CreateProductInput, RequestExecutionInput,
        };
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
            .request_execution(RequestExecutionInput {
                work_item_id: chore.id.clone(),
                priority: None,
                preferred_workspace_id: None,
                force: false,
            })
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
        let resolver: std::sync::Arc<dyn TranscriptPathResolver> =
            std::sync::Arc::new(NopResolver);
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
            .request_execution(RequestExecutionInput {
                work_item_id: chore.id.clone(),
                priority: None,
                preferred_workspace_id: None,
                force: false,
            })
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
            .request_execution(RequestExecutionInput {
                work_item_id: chore.id.clone(),
                priority: None,
                preferred_workspace_id: None,
                force: false,
            })
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
            <ServerState as TranscriptPathResolver>::transcript_path(
                &server_state,
                &execution.id,
            )
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

        let resolved = <ServerState as TranscriptPathResolver>::transcript_path(
            &server_state,
            &execution.id,
        )
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
        let wrong = <ServerState as TranscriptPathResolver>::transcript_path(
            &server_state,
            &run.id,
        )
        .await;
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
            .request_execution(RequestExecutionInput {
                work_item_id: chore.id.clone(),
                priority: None,
                preferred_workspace_id: None,
                force: false,
            })
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
        server_state
            .live_worker_states
            .register_spawn(slot_id, execution.id.clone(), "claude-opus-4-7", 0, None);

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
            .request_execution(RequestExecutionInput {
                work_item_id: chore.id.clone(),
                priority: None,
                preferred_workspace_id: None,
                force: false,
            })
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
        server_state
            .worker_registry
            .register_run_slot("run-a", 1);
        server_state
            .worker_registry
            .register_run_slot("run-b", 2);
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
                    FrontendEvent::EngineRequest { request_id, request } => match request {
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

        app_responder
            .await
            .expect("app responder task panicked");

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
            .update_work_item(&chore_id, WorkItemPatch { status: Some("done".into()), ..Default::default() })
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
                WorkItemPatch { status: Some("in_review".into()), ..Default::default() },
            )
            .unwrap();
        assert!(
            in_review_chore_execution(&db, &item).is_none(),
            "must return None when the chore has no executions"
        );
    }

    #[test]
    fn in_review_chore_execution_returns_execution_id_when_in_review() {
        use boss_protocol::WorkItemPatch;
        use crate::work::{CreateExecutionInput};
        let (db, _, chore_id) = make_work_db_with_chore();
        // Create an execution for the chore.
        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore_id.clone(),
                kind: "chore_implementation".into(),
                status: Some("ready".into()),
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
                prefer_is_soft: false,
                pr_url: None,
            })
            .unwrap();
        let item = db
            .update_work_item(
                &chore_id,
                WorkItemPatch { status: Some("in_review".into()), ..Default::default() },
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
        let msg =
            build_chore_update_message("name", "name", "old description", "new description")
                .expect("should produce a message");
        assert!(msg.contains("[chore-update]"));
        assert!(msg.contains("old description"));
        assert!(msg.contains("new description"));
    }

    #[test]
    fn build_chore_update_message_includes_both_when_both_change() {
        let msg =
            build_chore_update_message("old name", "new name", "old desc", "new desc")
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
        server_state
            .worker_registry
            .register_run_slot(run_id, 4);
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
        let msg = build_chore_update_message(
            &old_name,
            &new_name,
            &old_description,
            &new_description,
        )
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
}
