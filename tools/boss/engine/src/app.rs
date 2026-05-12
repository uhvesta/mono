use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify, oneshot};

use crate::cli::Cli;
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
    ReleaseWorkerPaneInput, RequestExecutionInput, SendToPaneInput, TOPIC_WORK_PRODUCTS,
    TOPIC_WORKER_LIVE_STATES, TopicEventPayload, execution_topic, probe_topic, work_product_topic,
};
use crate::work::{Task, WorkDb, WorkItem};
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
        match self.work_db.get_run(run_id) {
            Ok(run) => run.transcript_path.map(std::path::PathBuf::from),
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
        // caller. Discard it here.
        let _ = server.queue_probe(run_id.to_owned(), text.to_owned());
    }
}

/// One queued probe that has not yet been dispatched into the worker.
#[derive(Debug, Clone)]
struct PendingProbe {
    probe_id: String,
    text: String,
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
    /// Snapshot of the Anthropic API key captured at engine startup.
    /// Used by the live-status summarizer for the per-slot task; the
    /// pane-titlebar summarizer continues to resolve the key
    /// per-spawn via `cfg.agent()`.
    anthropic_api_key: Option<String>,
    next_session_id: AtomicU64,
    work_revision: Arc<AtomicU64>,
    /// Pid of the process the engine trusts as the macOS app — must
    /// match a session's `peer_pid` for `RegisterAppSession` to
    /// succeed. `None` only in tests; production sets this from
    /// `getppid()` at startup.
    app_pid: Option<libc::pid_t>,
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
    /// Weak self-reference produced by `Arc::new_cyclic`. Kept so
    /// late-bound consumers (the pane-spawn runner) can resolve back
    /// to the live `Arc<ServerState>` without an outer allocation.
    _self_weak: Weak<ServerState>,
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

impl ServerState {
    fn new_arc(cfg: Arc<RuntimeConfig>) -> Result<Arc<Self>> {
        let app_pid = current_parent_pid();
        Self::new_arc_with_app_pid(cfg, app_pid)
    }

    fn new_arc_with_app_pid(
        cfg: Arc<RuntimeConfig>,
        app_pid: Option<libc::pid_t>,
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
        tracing::info!(
            engine_build_sha = crate::build_info::git_sha(),
            engine_build_time = crate::build_info::build_time(),
            "live_status: engine starting (build identity)",
        );
        let worker_pool = WorkerPool::new(cfg.work.worker_pool_size);
        let topic_broker = Arc::new(TopicBroker::default());
        let work_revision = Arc::new(AtomicU64::new(0));
        let publisher: Arc<dyn ExecutionPublisher> = Arc::new(BrokerExecutionPublisher {
            topic_broker: topic_broker.clone(),
            work_revision: work_revision.clone(),
        });
        let cube_client: Arc<dyn CubeClient> = Arc::new(CommandCubeClient::new(cfg.clone()));
        let pr_detector: Arc<dyn PrDetector> = Arc::new(CommandPrDetector::new());
        // The pane releaser and probe queuer both need a Weak<ServerState>
        // to call back into ServerState methods, so they're late-bound
        // after the Arc<ServerState> exists. Same pattern as
        // `PaneSpawnRunner` below.
        let pane_releaser = Arc::new(ServerStatePaneReleaser::default());
        let probe_queuer = Arc::new(ServerStateProbeQueuer::default());
        let completion_handler = Arc::new(WorkerCompletionHandler::new(
            work_db.clone(),
            pr_detector,
            cube_client.clone(),
            publisher.clone(),
            pane_releaser.clone(),
            probe_queuer.clone(),
        ));

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

        // Resolve the Boss state root from the db path's parent so the
        // dispatch-event JSONL stream lands next to state.db /
        // events.sock under the same root. Falls back to the user's
        // `~/Library/Application Support/Boss/` if the db path has no
        // parent (only possible in degenerate test configs).
        let dispatch_event_root: PathBuf = cfg
            .work
            .db_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(|home| PathBuf::from(home).join("Library/Application Support/Boss"))
                    .unwrap_or_else(|| PathBuf::from("."))
            });
        let dispatch_events: Arc<dyn crate::dispatch_events::DispatchEventSink> = Arc::new(
            crate::dispatch_events::JsonlFileSink::new(dispatch_event_root),
        );

        let server_state = Arc::new_cyclic(move |weak_self: &Weak<ServerState>| {
            let mut execution_coordinator_inner = ExecutionCoordinator::with_publisher(
                work_db.clone(),
                worker_pool,
                cube_client,
                runner_for_coordinator,
                publisher,
            );
            execution_coordinator_inner.set_dispatch_events(dispatch_events);
            let execution_coordinator = Arc::new(execution_coordinator_inner);

            ServerState {
                work_db,
                execution_coordinator,
                completion_handler,
                cube_client: cube_client_for_state,
                publisher: publisher_for_state,
                topic_broker,
                worker_registry: WorkerRegistry::new(),
                live_worker_states: Arc::new(LiveWorkerStateRegistry::new()),
                live_status_manager: Arc::new(LiveStatusManager::new()),
                anthropic_api_key,
                next_session_id: AtomicU64::new(1),
                work_revision,
                app_pid,
                boss_pid: StdMutex::new(None),
                pending_probes: StdMutex::new(HashMap::new()),
                in_flight_probes: StdMutex::new(HashMap::new()),
                next_probe_id: AtomicU64::new(1),
                app_session: Arc::new(Mutex::new(None)),
                spawn_pane_lock: Arc::new(Mutex::new(())),
                _self_weak: weak_self.clone(),
            }
        });

        // Late-bind the runner to the Arc<ServerState>. Going through
        // the WorkerSpawner trait keeps the runner unaware of
        // ServerState's private fields.
        let weak_spawner: Weak<dyn crate::spawn_flow::WorkerSpawner> =
            Arc::downgrade(&server_state) as Weak<dyn crate::spawn_flow::WorkerSpawner>;
        pane_runner.set_server_state(weak_spawner);
        pane_releaser.set_server_state(Arc::downgrade(&server_state));
        probe_queuer.set_server_state(Arc::downgrade(&server_state));

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
        // Always drop the live-state entry — we've already given up
        // ownership of the slot in the worker registry, so a stale
        // entry here would lie to the UI about the slot being live.
        self.live_worker_states.release_slot(slot_id);
        // Tear down the per-slot live-status task. The manager
        // doesn't await the task's exit so a wedged Anthropic call
        // can't block the release path.
        self.live_status_manager.stop_slot(slot_id);
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

    /// Push probe text onto the FIFO for `run_id`, mint a fresh
    /// `probe_id`, and return it so the caller can correlate the
    /// queued probe with the eventual `FrontendEvent::ProbeReplied`
    /// push. Multiple probes for the same run queue in order; the
    /// events-socket consumer pops one per `Stop` hook event.
    pub fn queue_probe(&self, run_id: String, text: String) -> String {
        let probe_id = self.allocate_probe_id();
        self.pending_probes
            .lock()
            .expect("pending_probes mutex poisoned")
            .entry(run_id)
            .or_default()
            .push_back(PendingProbe {
                probe_id: probe_id.clone(),
                text,
            });
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
        let app_pid = self.app_pid;
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

struct BrokerExecutionPublisher {
    topic_broker: Arc<TopicBroker>,
    work_revision: Arc<AtomicU64>,
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

pub async fn run(cli: Cli) -> Result<()> {
    let cfg = Arc::new(RuntimeConfig::load_from_env()?);
    tracing::info!(
        cwd = %cfg.work.cwd.display(),
        db_path = %cfg.work.db_path.display(),
        "starting boss-engine runtime",
    );

    run_server(cli, cfg).await
}

async fn run_server(cli: Cli, cfg: Arc<RuntimeConfig>) -> Result<()> {
    let socket_path = cli
        .socket_path
        .unwrap_or_else(|| DEFAULT_SOCKET_PATH.to_owned());
    let pid_file_path =
        std::env::var("BOSS_ENGINE_PID_PATH").unwrap_or_else(|_| DEFAULT_PID_PATH.to_owned());
    let events_socket_path = default_events_socket_path()?;
    serve(
        cfg,
        socket_path.into(),
        Some(pid_file_path.into()),
        Some(events_socket_path),
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

/// Run the frontend server until the listener fails.
///
/// `socket_path` is bound exclusively (the file is removed first if it exists).
/// When `pid_file_path` is `Some`, the engine writes its pid there and removes
/// the file on shutdown — pass `None` from in-process tests to avoid touching
/// shared filesystem state. When `events_socket_path` is `Some`, the engine
/// also binds the worker events socket (mode 0600) and runs an accept loop
/// that decodes hook payloads via the worker registry; pass `None` from
/// tests that don't exercise the events channel.
pub async fn serve(
    cfg: Arc<RuntimeConfig>,
    socket_path: std::path::PathBuf,
    pid_file_path: Option<std::path::PathBuf>,
    events_socket_path: Option<std::path::PathBuf>,
) -> Result<()> {
    let server_state = ServerState::new_arc(cfg.clone())?;

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
            tracing::warn!(
                count = healed.len(),
                ids = ?healed,
                "demoted ghost-active chores with no run history",
            );
        }
        Ok(_) => {
            tracing::debug!("no ghost-active chores to demote at startup");
        }
        Err(err) => {
            tracing::error!(?err, "ghost-active sweep failed; continuing");
        }
    }

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
        Duration::from_secs(60),
    );

    let coordinator = server_state.execution_coordinator.clone();
    coordinator.kick();

    install_panic_hook(&server_state);

    tracing::info!(
        socket_path = %socket_path.display(),
        "frontend socket: accept loop started",
    );
    crate::audit::record_accept_loop_started("frontend", &socket_path);

    loop {
        tokio::select! {
            biased;
            signal = graceful_shutdown_signal() => {
                tracing::info!(signal, "shutdown signal received; releasing worker panes");
                crate::audit::record_shutdown(format!("signal:{signal}"));
                server_state
                    .shutdown_workers(Duration::from_secs(5), Duration::from_secs(1))
                    .await;
                tracing::info!("engine shutdown complete");
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
                    let registry = server_state.worker_registry.clone();
                    match handle_connection(stream, &registry).await {
                        Ok(incoming) => {
                            tracing::info!(
                                peer_pid = ?incoming.peer_pid,
                                run_id = ?incoming.run_id,
                                event = ?incoming.event,
                                "events socket: hook event received",
                            );
                            dispatch_live_worker_state(&server_state, &incoming).await;
                            // ProbeReplied runs *before* dispatch so a
                            // single Stop never both fires the reply
                            // for the prior probe and the dispatch of
                            // the next one — without the ordering, the
                            // probe-just-dispatched would be picked up
                            // for emission immediately, with no reply
                            // text actually written yet.
                            dispatch_probe_reply_on_stop(&server_state, &incoming).await;
                            dispatch_probe_on_stop(&server_state, &incoming).await;
                            dispatch_completion_on_stop(&server_state, &incoming).await;
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
    tracing::info!(
        run_id = ?incoming.run_id,
        peer_pid = ?incoming.peer_pid,
        kind = event_kind,
        has_transcript_path = incoming.transcript_path.is_some(),
        "live_status: hook payload arrived at dispatcher",
    );
    let Some(run_id) = incoming.run_id.as_deref() else {
        tracing::warn!(
            kind = event_kind,
            peer_pid = ?incoming.peer_pid,
            "live_status: dropping hook — neither _boss_run_id payload nor peer-pid ancestor walk produced a run_id",
        );
        return;
    };
    let Some(slot_id) = server_state.worker_registry.slot_for_run(run_id) else {
        tracing::warn!(
            run_id,
            kind = event_kind,
            "live_status: dropping hook — run_id is not registered against a slot (event ahead of register_run_slot or after take_slot_for_run)",
        );
        return;
    };
    // Persist the transcript path the moment we see it on a hook
    // payload. `start_execution_run` inserts the work_runs row with
    // `transcript_path = NULL` (the engine has no way to know the
    // path until the worker tells us via its first hook), so without
    // this write the live-status summarizer's `TranscriptPathResolver`
    // returns None forever and the per-slot loop early-outs every
    // tick on "no transcript path yet". The setter is idempotent
    // (first-writer-wins) so we don't clobber the path the tail
    // watcher has already opened across later sessions/resumes.
    if let Some(path) = incoming.transcript_path.as_deref() {
        match server_state.work_db.set_run_transcript_path_if_unset(run_id, path) {
            Ok(true) => tracing::info!(
                run_id,
                slot_id,
                transcript_path = %path,
                "recorded transcript_path on work_run from hook payload",
            ),
            Ok(false) => {}
            Err(err) => tracing::warn!(
                run_id,
                slot_id,
                ?err,
                "failed to persist transcript_path from hook payload",
            ),
        }
    }
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
        crate::protocol::WorkerEvent::PostToolUse { .. } => {
            server_state
                .live_status_manager
                .notify(slot_id, Trigger::PostToolUse);
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

/// Look up the transcript path the run is currently writing to (via
/// `WorkRun`), and stat its current byte size so we can use that as
/// the lower bound for the next reply-extraction read. Returns
/// `(None, 0)` when the run has no transcript path recorded yet —
/// the in-flight bookkeeping still tracks the dispatched probe, but
/// `dispatch_probe_reply_on_stop` will skip emission with a warning
/// rather than fabricate empty reply text.
async fn transcript_offset_for_run(
    server_state: &Arc<ServerState>,
    run_id: &str,
) -> (Option<String>, u64) {
    let path = match server_state.work_db.get_run(run_id) {
        Ok(run) => run.transcript_path,
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
/// kanban state. Runs after `dispatch_probe_on_stop` so probe
/// injection (which keeps the worker working) wins over completion
/// (which assumes the worker is idle).
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
    tracing::debug!(run_id, ?outcome, "completion handler stop result");
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
            FrontendRequest::GetWorkItem { id } => match work_db.get_work_item(&id) {
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
                        FrontendEvent::WorkError {
                            message: err.to_string(),
                        },
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
                        FrontendEvent::WorkError {
                            message: err.to_string(),
                        },
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
                match work_db.update_work_item(&id, patch) {
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
                        if task_transitioned_to_active(&previous_task_status, &item)
                            && work_item_needs_dispatch(&work_db, &work_item_id(&item))
                        {
                            let live_states = server_state.live_worker_states.clone();
                            let dispatch_input = RequestExecutionInput {
                                work_item_id: work_item_id(&item),
                                priority: None,
                                preferred_workspace_id: None,
                                force: false,
                            };
                            match work_db.request_execution_with_live_check(
                                dispatch_input,
                                |run_id| live_states.is_run_live(run_id),
                            ) {
                                Ok(_) => {
                                    server_state.execution_coordinator.kick();
                                }
                                Err(err) => {
                                    tracing::warn!(
                                        work_item_id = %work_item_id(&item),
                                        ?err,
                                        "UpdateWorkItem → active: auto-dispatch failed; \
                                         status update kept, no worker spawned",
                                    );
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
            FrontendRequest::GetRun { id } => match work_db.get_run(&id) {
                Ok(run) => {
                    send_response(&sink, &request_id, FrontendEvent::RunResult { run });
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
            FrontendRequest::RegisterAppSession => {
                // Trust the peer if either:
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
                let engine_pid = std::process::id() as libc::pid_t;
                let trust_ok = match (server_state.app_pid, peer_pid) {
                    (None, _) => true, // tests / no-trust-root mode
                    (Some(expected), Some(observed)) => {
                        observed == expected || is_descendant_of_any(engine_pid, &[observed])
                    }
                    (Some(_), None) => false,
                };
                if !trust_ok {
                    tracing::warn!(
                        peer_pid = ?peer_pid,
                        engine_pid,
                        expected_app_pid = ?server_state.app_pid,
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
            FrontendRequest::ProbeRun { run_id, text } => {
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
                let probe_id = server_state.queue_probe(run_id.clone(), text);
                tracing::info!(run_id = %run_id, probe_id = %probe_id, "probe queued");
                send_response(&sink, &request_id, FrontendEvent::ProbeQueued { run_id, probe_id });
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
                    handler.force_release(&run_id_for_release).await;
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
            FrontendRequest::ListWorkerLiveStates => {
                let states = server_state.live_worker_states_snapshot();
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::WorkerLiveStatesList { states },
                );
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
                match server_state.work_db.get_run(&run_id) {
                    Ok(run) => {
                        let Some(transcript_path) = run.transcript_path.clone() else {
                            send_response(
                                &sink,
                                &request_id,
                                FrontendEvent::WorkError {
                                    message: format!(
                                        "run {run_id} has no transcript path recorded"
                                    ),
                                },
                            );
                            continue;
                        };
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
                let leased_repos: HashSet<String> = work_db
                    .list_in_flight_executions()
                    .map(|execs| execs.into_iter().map(|e| e.repo_remote_url).collect())
                    .unwrap_or_default();
                match work_db
                    .resolve_project_design_doc(&project_id, |repo| leased_repos.contains(repo))
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
        }
    }

    server_state.topic_broker.remove_session(&session_id).await;
    server_state.drop_app_session_if_matches(&session_id).await;
    sink.close();
    let _ = writer_task.await;
    Ok(())
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
        let transcript_path = snap
            .transcript_path
            .clone()
            .or_else(|| {
                let run_id = live.map(|s| s.run_id.as_str())?;
                work_db
                    .get_run(run_id)
                    .ok()
                    .and_then(|r| r.transcript_path)
            });
        slots.push(LiveStatusSlotDebug {
            slot_id,
            task_running: active_slots.contains(&slot_id),
            disabled: disabled_set.contains(&slot_id),
            last_trigger_kind: snap.last_trigger_kind.clone(),
            last_trigger_at: snap.last_trigger_at_epoch_s.map(format_epoch_iso8601),
            last_outcome_tag: snap.last_outcome_tag.clone(),
            last_outcome_detail: snap.last_outcome_detail.clone(),
            last_outcome_at: snap.last_outcome_at_epoch_s.map(format_epoch_iso8601),
            last_success_at: snap.last_success_at_epoch_s.map(format_epoch_iso8601),
            last_success_text: snap.last_success_text.clone(),
            transcript_path,
            last_redacted_bytes: snap.last_redacted_bytes.map(|n| n as u64),
        });
    }

    LiveStatusDebugReport {
        engine_build_sha: crate::build_info::git_sha().to_owned(),
        engine_build_time: crate::build_info::build_time().to_owned(),
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
            send_response(
                sink,
                request_id,
                FrontendEvent::WorkError {
                    message: format!("{err:#}"),
                },
            );
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

/// Read the trailing `lines` lines of `transcript_path`. Returns the
/// raw line contents (no trailing newline) plus a flag indicating
/// whether the file held more lines than were returned.
///
/// The transcript file is expected to be JSONL the worker writes
/// incrementally; this helper does not parse it, so the caller can
/// decide how to render. A missing file is reported as an io error
/// instead of returning an empty result so callers can distinguish
/// "no transcript yet" from "transcript is empty".
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
        ServerState::new_arc_with_app_pid(cfg, None).unwrap()
    }

    fn make_session_sink() -> Arc<SessionSink> {
        let (shutdown_tx, _shutdown_rx) = oneshot::channel::<()>();
        Arc::new(SessionSink::new(shutdown_tx))
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
        ServerState::new_arc_with_app_pid(cfg, Some(app_pid)).unwrap()
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
        // itself as a worker so the ancestor walk hits on step zero;
        // app_pid is set to a clearly bogus value (1) so the trust
        // subtree check fails first.
        let server_state = server_state_with_app_pid(1);
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
        let id_one = server_state.queue_probe("run-x".into(), "first".into());
        let id_two = server_state.queue_probe("run-x".into(), "second".into());
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
        let run = server_state
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

        // Map the run to slot 1 so dispatch_probe_on_stop has a target
        // for `SendToPane`.
        server_state.worker_registry.register_run_slot(run.id.clone(), 1);

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
            .subscribe(&session_id, &[probe_topic(&run.id)])
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
        let probe_id = server_state.queue_probe(run.id.clone(), "what now?".into());

        // Fire the first Stop boundary. This dispatches the probe to
        // the (fake) app session and records the in-flight entry.
        let first_stop = crate::events_socket::IncomingHookEvent {
            peer_pid: None,
            run_id: Some(run.id.clone()),
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
            run_id: Some(run.id.clone()),
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
                assert_eq!(emitted_run, run.id);
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
}
