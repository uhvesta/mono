use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::sync::{Arc, Weak};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify, oneshot};

use crate::acp::{AcpClient, AcpEvent};
use crate::cli::{Cli, Mode};
use crate::completion::{
    CommandPrDetector, PrDetector, ProbeQueuer, WorkerCompletionHandler, WorkerPaneReleaser,
};
use crate::config::RuntimeConfig;
use crate::events_socket::{bind_events_socket, handle_connection, peer_pid};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::merge_poller::{CommandMergeProbe, MergeProbe, spawn_loop as spawn_merge_poller};
use crate::worker_registry::WorkerRegistry;
use crate::coordinator::{
    CommandCubeClient, CubeClient, ExecutionCoordinator, ExecutionPublisher, WorkerPool,
};
use crate::protocol::{
    AgentInfo, AgentRole, EngineToAppError, EngineToAppRequest, EngineToAppResponse,
    FocusWorkerPaneInput, FrontendEvent, FrontendEventEnvelope, FrontendRequest,
    FrontendRequestEnvelope, ReleaseWorkerPaneInput, TOPIC_WORK_PRODUCTS,
    TOPIC_WORKER_LIVE_STATES, TopicEventPayload, execution_topic, work_product_topic,
};
use tokio::time::{Duration, timeout};
use crate::runner::AcpExecutionRunner;
use crate::work::{WorkDb, WorkItem};
use async_trait::async_trait;

const DEFAULT_SOCKET_PATH: &str = "/tmp/boss-engine.sock";
const DEFAULT_PID_PATH: &str = "/tmp/boss-engine.pid";
const BOSS_AGENT_NAME: &str = "The Boss";
const BOSS_AGENT_SYSTEM_PROMPT: &str = r#"You are The Boss, the overall coordinator and primary interface with the user inside Boss.

Your role is to coordinate work and keep Boss's representation of work accurate.

You may use the `boss` CLI to create and update products, projects, tasks, and chores when the user explicitly asks for that work or when it is strongly implied by the request. Prefer non-interactive CLI usage when possible. Infer the most likely work item shape yourself. Ask a concise clarifying question only when you truly cannot infer a usable product or the request is impossible to place without more information.

If a user request looks like implementation work, bug fixing, feature work, cleanup, follow-up work, or investigation that might turn into a task or project, do not inspect the repository or perform detailed technical analysis before capturing it in Boss. Queue the work first.

Treat investigation, scoping, and discovery as work items for another agent. If the user asks to investigate something, or if investigation is the obvious next step, create an investigation task or project instead of doing the investigation yourself.

When work is strongly implied, bias toward creating the appropriate Boss work item quickly, even if some implementation details are still unknown. If you are uncertain, make the best inference and create the item anyway rather than asking the user to choose the type.

Use the current Boss UI context, especially the current product and its existing projects, when deciding how to represent work.

When you need authoritative Boss CLI syntax or selector/status rules, use `boss reference --json --no-input`. Treat that output as the current source of truth for this build. Do not use `boss ... --help` for syntax discovery unless `boss reference` is unavailable.

Routing rules:
- If there is a current selected product, use that product by default unless the user clearly names a different product.
- If the request clearly fits an existing project, create a task in that project instead of creating a new project or a chore.
- If the request does not fit an existing project and seems small, self-contained, operational, or maintenance-oriented, create a chore.
- If the request does not fit an existing project and seems broad, ambiguous, exploratory, or likely to require multiple stages or multiple tasks, create a project.
- If the request is to investigate something and that investigation belongs under an existing project, create an investigation task in that project. Otherwise, prefer a new project when the investigation is broad or likely to branch into multiple follow-up tasks.
- If you are deciding between chore and project and both seem plausible, default to chore unless the work clearly looks multi-stage, broad, or exploratory.
- If you are deciding whether a small fix belongs in an existing project and there is no obvious fit, default to chore.
- Do not ask the user whether something should be a task, chore, or project when a reasonable inference is available. It is acceptable to be wrong because the work can be moved later.

Do not make direct implementation changes yourself. Do not edit code, modify files, or carry out the underlying work directly unless the user explicitly overrides this rule. Instead, act as the coordinator of the work and the steward of its representation in Boss.

After creating a work item, the Boss engine auto-dispatches a worker on it. Do not ask the user whether to dispatch a worker now or leave it in the backlog — that question is always redundant. Do not append generic follow-ups like "Want me to dispatch a worker on it now, or leave it in the backlog?". A successful creation reply should simply state that the item was queued (id and status) and stop. Only surface a follow-up when there is a specifically-actionable issue: dispatch failed, configuration is missing, a sequencing or dependency decision is needed, or the user genuinely has to choose between concrete options. Never invent a follow-up question for the sake of offering one.

Default behavior:
- clarify goals and scope,
- queue likely work immediately, including investigation work,
- ask only when you cannot reasonably infer the destination product or representation,
- use the current product and existing project context before choosing task, chore, or project,
- avoid repo inspection and detailed technical analysis before the work is queued,
- keep status and structure accurate,
- suggest or assign implementation and investigation work rather than doing it yourself."#;

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
        server.queue_probe(run_id.to_owned(), text.to_owned());
    }
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

struct Agent {
    id: String,
    name: String,
    role: AgentRole,
    acp_client: Arc<AcpClient>,
    session_id: String,
    prompt_lock: Arc<Mutex<()>>,
    system_prompt: Option<String>,
}

struct AgentRegistry {
    agents: Mutex<HashMap<String, Agent>>,
    next_id: AtomicU64,
    cfg: Arc<RuntimeConfig>,
}

impl AgentRegistry {
    fn new(cfg: Arc<RuntimeConfig>) -> Self {
        Self {
            agents: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            cfg,
        }
    }

    fn allocate_agent(&self, name: Option<String>, role: AgentRole) -> (String, String, AgentRole) {
        let id = format!("agent-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let name = match role {
            AgentRole::Boss => name.unwrap_or_else(|| BOSS_AGENT_NAME.to_owned()),
            AgentRole::Standard => name
                .unwrap_or_else(|| format!("Agent {}", id.strip_prefix("agent-").unwrap_or(&id))),
        };
        (id, name, role)
    }

    async fn initialize_agent(&self, id: &str, name: &str, role: AgentRole) -> Result<()> {
        let acp_client = Arc::new(AcpClient::connect_with_external_permissions(&self.cfg).await?);
        acp_client.initialize().await?;
        let session_id = acp_client.new_session(&self.cfg.work.cwd).await?;
        let system_prompt = system_prompt_for_role(role);

        tracing::info!(
            agent_id = %id,
            name = %name,
            ?role,
            session_id = %session_id,
            "agent ready"
        );

        let agent = Agent {
            id: id.to_owned(),
            name: name.to_owned(),
            role,
            acp_client,
            session_id,
            prompt_lock: Arc::new(Mutex::new(())),
            system_prompt,
        };

        self.agents.lock().await.insert(id.to_owned(), agent);
        Ok(())
    }

    async fn remove_agent(&self, agent_id: &str) -> Result<()> {
        let removed = self.agents.lock().await.remove(agent_id);
        if removed.is_none() {
            bail!("unknown agent: {agent_id}");
        }
        tracing::info!(agent_id = %agent_id, "agent removed");
        Ok(())
    }

    async fn list_agents(&self) -> Vec<AgentInfo> {
        self.agents
            .lock()
            .await
            .values()
            .map(|agent| AgentInfo {
                agent_id: agent.id.clone(),
                name: agent.name.clone(),
                role: agent.role,
            })
            .collect()
    }

    async fn get_acp_and_session(
        &self,
        agent_id: &str,
    ) -> Result<(Arc<AcpClient>, String, Arc<Mutex<()>>, Option<String>)> {
        let agents = self.agents.lock().await;
        let agent = agents
            .get(agent_id)
            .with_context(|| format!("unknown agent: {agent_id}"))?;
        Ok((
            agent.acp_client.clone(),
            agent.session_id.clone(),
            agent.prompt_lock.clone(),
            agent.system_prompt.clone(),
        ))
    }
}

fn system_prompt_for_role(role: AgentRole) -> Option<String> {
    match role {
        AgentRole::Standard => None,
        AgentRole::Boss => Some(BOSS_AGENT_SYSTEM_PROMPT.to_owned()),
    }
}

fn compose_agent_prompt(system_prompt: Option<&str>, user_text: &str) -> String {
    match system_prompt {
        // The current ACP prompt surface is plain text only, so role-specific
        // instructions are wrapped into each prompt instead of being sent over
        // a dedicated system channel.
        Some(system_prompt) => {
            format!("<system>\n{system_prompt}\n</system>\n\n<user>\n{user_text}\n</user>")
        }
        None => user_text.to_owned(),
    }
}

struct ServerState {
    work_db: Arc<WorkDb>,
    agent_registry: Arc<AgentRegistry>,
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
    /// Pending probe text per run, FIFO. The events-socket consumer
    /// pops one entry per `Stop` hook event for the matching run and
    /// dispatches it as `SendToPane` to the app.
    pending_probes: StdMutex<HashMap<String, VecDeque<String>>>,
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
///   to be — Boss pane, app shell, or *inside a worker pane* — and
///   `AppOrBoss` admits all of those (workers are siblings under
///   the app).
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

        let server_state = Arc::new_cyclic(move |weak_self: &Weak<ServerState>| {
            let execution_coordinator = Arc::new(ExecutionCoordinator::with_publisher(
                work_db.clone(),
                worker_pool,
                cube_client,
                runner_for_coordinator,
                publisher,
            ));

            ServerState {
                work_db,
                agent_registry: Arc::new(AgentRegistry::new(cfg.clone())),
                execution_coordinator,
                completion_handler,
                cube_client: cube_client_for_state,
                publisher: publisher_for_state,
                topic_broker,
                worker_registry: WorkerRegistry::new(),
                live_worker_states: Arc::new(LiveWorkerStateRegistry::new()),
                next_session_id: AtomicU64::new(1),
                work_revision,
                app_pid,
                boss_pid: StdMutex::new(None),
                pending_probes: StdMutex::new(HashMap::new()),
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
            handle.sink.enqueue(FrontendEventEnvelope::push(
                FrontendEvent::EngineRequest {
                    request_id: request_id.clone(),
                    request: request.clone(),
                },
            ));
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
        self.broadcast_live_worker_states().await;
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

    /// Push probe text onto the FIFO for `run_id`. Multiple probes for
    /// the same run queue in order; the events-socket consumer pops
    /// one per `Stop` hook event.
    pub fn queue_probe(&self, run_id: String, text: String) {
        self.pending_probes
            .lock()
            .expect("pending_probes mutex poisoned")
            .entry(run_id)
            .or_default()
            .push_back(text);
    }

    /// Pop the next pending probe for `run_id`, if any. Called from
    /// the events-socket consumer when a `Stop` event arrives.
    pub fn pop_pending_probe(&self, run_id: &str) -> Option<String> {
        let mut guard = self
            .pending_probes
            .lock()
            .expect("pending_probes mutex poisoned");
        let queue = guard.get_mut(run_id)?;
        let text = queue.pop_front();
        if queue.is_empty() {
            guard.remove(run_id);
        }
        text
    }

    /// Authorize a peer-pid against an RPC tier. Walks up the peer's
    /// process tree (bounded depth) looking for `app_pid` or
    /// `boss_pid` registered as a trust root.
    ///
    /// Returns `true` when `tier == User`, when the trust root is
    /// `None` (test mode), or when an ancestor of `peer_pid` matches
    /// a relevant trust root.
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
                let trust_set: Vec<libc::pid_t> =
                    [app_pid, boss_pid].into_iter().flatten().collect();
                if trust_set.is_empty() {
                    return false;
                }
                is_descendant_of_any(peer_pid, &trust_set)
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
    async fn publish(
        &self,
        execution_id: &str,
        work_item_id: &str,
        status: &str,
        reason: &str,
    ) {
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

    async fn publish_work_item_changed(
        &self,
        product_id: &str,
        work_item_id: &str,
        reason: &str,
    ) {
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
                EnqueueOutcome::Enqueued
                | EnqueueOutcome::Coalesced
                | EnqueueOutcome::Closed => {}
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

    match cli.mode {
        Mode::Cli => run_cli(cli, cfg).await,
        Mode::Server => run_server(cli, cfg).await,
    }
}

async fn run_cli(cli: Cli, cfg: Arc<RuntimeConfig>) -> Result<()> {
    let agent = cfg.agent()?;
    agent.preflight_acp()?;
    let acp = AcpClient::connect(&cfg).await?;
    acp.initialize().await?;
    let session_id = acp.new_session(&cfg.work.cwd).await?;

    println!("Connected to ACP adapter. Session: {session_id}");

    if let Some(prompt) = cli.prompt {
        run_prompt(&acp, &session_id, &prompt).await?;
        return Ok(());
    }

    println!("Enter a prompt (Ctrl-D to exit):");
    print!("> ");
    std::io::stdout().flush()?;

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let prompt = line.trim();
        if prompt.is_empty() {
            print!("> ");
            std::io::stdout().flush()?;
            continue;
        }

        run_prompt(&acp, &session_id, prompt).await?;
        println!();
        print!("> ");
        std::io::stdout().flush()?;
    }

    Ok(())
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

    if socket_path.exists() {
        tokio::fs::remove_file(&socket_path)
            .await
            .with_context(|| format!("failed to remove existing socket {}", socket_path.display()))?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind unix socket {}", socket_path.display()))?;

    let _pid_guard = match pid_file_path {
        Some(path) => {
            let path_str = path.to_string_lossy().into_owned();
            let pid = std::process::id();
            std::fs::write(&path, format!("{pid}\n"))
                .with_context(|| format!("failed to write pid file {path_str}"))?;
            tracing::info!(pid, pid_file = %path_str, "engine pid file is ready");
            Some(PidFileGuard { path: path_str, pid })
        }
        None => None,
    };

    tracing::info!(socket_path = %socket_path.display(), "frontend socket is ready");
    println!("boss-engine listening on {}", socket_path.display());

    if let Some(path) = events_socket_path {
        let events_listener = bind_events_socket(&path)
            .with_context(|| format!("failed to bind events socket {}", path.display()))?;
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
    // On startup the live-worker registry is empty (no worker has
    // spawned yet), so passing `is_run_live` here treats every
    // existing non-terminal execution as stale — exactly what we want
    // after a crash, where every recorded `waiting_human` row is by
    // definition orphaned.
    let live_states_for_reconcile = server_state.live_worker_states.clone();
    match server_state
        .work_db
        .reconcile_active_dispatch(|run_id| live_states_for_reconcile.is_run_live(run_id))
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

    loop {
        let (stream, _) = listener.accept().await.context("socket accept failed")?;
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
    // Explicit override wins. Necessary when the engine is launched
    // through `bazel run`, which daemonizes its server: the engine
    // binary's actual parent ends up being `bazel`, reparented to
    // launchd, so `getppid()` and any ancestor walk both miss the
    // real app entirely. The macOS app sets `BOSS_APP_PID` to its own
    // pid before spawning the engine.
    if let Ok(raw) = std::env::var("BOSS_APP_PID") {
        if let Ok(parsed) = raw.parse::<libc::pid_t>() {
            if parsed > 1 {
                return Some(parsed);
            }
        }
    }
    // SAFETY: getppid() has no preconditions; the returned pid is
    // valid for the lifetime of the parent process (and thereafter
    // returns 1 / launchd, which we treat as no-app-detected via the
    // explicit check below).
    let ppid = unsafe { libc::getppid() };
    if ppid <= 1 { None } else { Some(ppid) }
}

async fn run_events_accept_loop(listener: UnixListener, server_state: Arc<ServerState>) {
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
async fn dispatch_live_worker_state(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
) {
    let Some(run_id) = incoming.run_id.as_deref() else {
        return;
    };
    let Some(slot_id) = server_state.worker_registry.slot_for_run(run_id) else {
        return;
    };
    let changed = server_state
        .live_worker_states
        .apply_event(slot_id, &incoming.event);
    if changed {
        server_state.broadcast_live_worker_states().await;
    }
}

/// On `Stop` hook events, pop a pending probe for the run (if any)
/// and `SendToPane` the text to the worker's slot. The injection
/// arrives at the pane just as the worker becomes idle, so claude
/// treats it as the next user prompt.
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
    let Some(text) = server_state.pop_pending_probe(run_id) else {
        return;
    };
    let Some(slot_id) = server_state.worker_registry.slot_for_run(run_id) else {
        tracing::warn!(
            run_id,
            "probe ready but no slot mapping; dropping probe text",
        );
        return;
    };
    let request = EngineToAppRequest::SendToPane(SendToPaneInput {
        slot_id,
        text: text.clone(),
    });
    match server_state
        .send_to_app(request, Duration::from_secs(5))
        .await
    {
        Ok(_) => {
            tracing::info!(run_id, slot_id, "probe injected into pane");
        }
        Err(err) => {
            tracing::warn!(
                ?err,
                run_id,
                slot_id,
                "probe injection failed; pushing text back onto queue",
            );
            // Push back on the front so the next Stop retries.
            server_state.queue_probe(run_id.to_owned(), text);
        }
    }
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
    let registry = server_state.agent_registry.clone();
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
                        agent_id: None,
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
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::Unsubscribed { topics },
                );
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
            FrontendRequest::ListProjects { product_id } => {
                match work_db.list_projects(&product_id) {
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
            } => match work_db.list_tasks(&product_id, project_id.as_deref()) {
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
            FrontendRequest::ListChores { product_id } => match work_db.list_chores(&product_id) {
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
            FrontendRequest::CreateTask { input } => match work_db.create_task(input) {
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
            },
            FrontendRequest::CreateChore { input } => match work_db.create_chore(input) {
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
            },
            FrontendRequest::UpdateWorkItem { id, patch } => {
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
                        if let Some(execution_id) =
                            terminal_chore_execution(&work_db, &item)
                        {
                            let handler = server_state.completion_handler.clone();
                            tokio::spawn(async move {
                                handler.force_release(&execution_id).await;
                            });
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
                                let coordinator =
                                    server_state.execution_coordinator.clone();
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
            FrontendRequest::CreateAgent { name, role } => {
                let (agent_id, agent_name, role) = registry.allocate_agent(name, role);
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::AgentCreated {
                        agent_id: agent_id.clone(),
                        name: agent_name.clone(),
                        role,
                    },
                );

                let sink = sink.clone();
                let registry = registry.clone();
                tokio::spawn(async move {
                    match registry
                        .initialize_agent(&agent_id, &agent_name, role)
                        .await
                    {
                        Ok(()) => {
                            send_push(&sink, FrontendEvent::AgentReady { agent_id });
                        }
                        Err(err) => {
                            tracing::error!(?err, agent_id = %agent_id, "failed to initialize agent");
                            send_push(
                                &sink,
                                FrontendEvent::Error {
                                    agent_id: Some(agent_id),
                                    message: format!("failed to initialize agent: {err}"),
                                },
                            );
                        }
                    }
                });
            }
            FrontendRequest::ListAgents => {
                let agents = registry.list_agents().await;
                send_response(&sink, &request_id, FrontendEvent::AgentList { agents });
            }
            FrontendRequest::RemoveAgent { agent_id } => {
                match registry.remove_agent(&agent_id).await {
                    Ok(()) => {
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::AgentRemoved { agent_id },
                        );
                    }
                    Err(err) => {
                        tracing::error!(?err, agent_id = %agent_id, "failed to remove agent");
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::Error {
                                agent_id: Some(agent_id),
                                message: err.to_string(),
                            },
                        );
                    }
                }
            }
            FrontendRequest::Prompt { agent_id, text } => {
                tracing::info!(
                    agent_id = %agent_id,
                    prompt_chars = text.chars().count(),
                    "received prompt from frontend"
                );

                let (acp, session_id, prompt_lock, system_prompt) =
                    match registry.get_acp_and_session(&agent_id).await {
                        Ok(tuple) => tuple,
                        Err(err) => {
                            send_response(
                                &sink,
                                &request_id,
                                FrontendEvent::Error {
                                    agent_id: Some(agent_id),
                                    message: err.to_string(),
                                },
                            );
                            continue;
                        }
                    };

                let sink = sink.clone();
                let agent_id_owned = agent_id.clone();
                let prompt_text = compose_agent_prompt(system_prompt.as_deref(), &text);

                tokio::spawn(async move {
                    let _guard = prompt_lock.lock().await;
                    let aid = agent_id_owned.clone();

                    let result = acp
                        .prompt_streaming(&session_id, &prompt_text, |event| match event {
                            AcpEvent::AgentMessageChunk { text, .. } => {
                                send_push(
                                    &sink,
                                    FrontendEvent::Chunk {
                                        agent_id: aid.clone(),
                                        text,
                                    },
                                );
                            }
                            AcpEvent::ToolCall { title, status, .. } => {
                                send_push(
                                    &sink,
                                    FrontendEvent::ToolCall {
                                        agent_id: aid.clone(),
                                        name: title,
                                        status: status.unwrap_or_else(|| "started".to_owned()),
                                    },
                                );
                            }
                            AcpEvent::ToolCallUpdate {
                                tool_call_id,
                                title,
                                status,
                                ..
                            } => {
                                let label = title.unwrap_or_else(|| {
                                    tool_call_id.unwrap_or_else(|| "tool".to_owned())
                                });
                                send_push(
                                    &sink,
                                    FrontendEvent::ToolCall {
                                        agent_id: aid.clone(),
                                        name: label,
                                        status: status.unwrap_or_else(|| "update".to_owned()),
                                    },
                                );
                            }
                            AcpEvent::PermissionRequest {
                                permission_id,
                                title,
                                ..
                            } => {
                                send_push(
                                    &sink,
                                    FrontendEvent::PermissionRequest {
                                        agent_id: aid.clone(),
                                        id: permission_id,
                                        title,
                                    },
                                );
                            }
                            AcpEvent::TerminalStarted {
                                id,
                                title,
                                command,
                                cwd,
                                ..
                            } => {
                                send_push(
                                    &sink,
                                    FrontendEvent::TerminalStarted {
                                        agent_id: aid.clone(),
                                        id,
                                        title,
                                        command,
                                        cwd,
                                    },
                                );
                            }
                            AcpEvent::TerminalOutput { id, text, .. } => {
                                send_push(
                                    &sink,
                                    FrontendEvent::TerminalOutput {
                                        agent_id: aid.clone(),
                                        id,
                                        text,
                                    },
                                );
                            }
                            AcpEvent::TerminalDone {
                                id,
                                exit_code,
                                signal,
                                ..
                            } => {
                                send_push(
                                    &sink,
                                    FrontendEvent::TerminalDone {
                                        agent_id: aid.clone(),
                                        id,
                                        exit_code,
                                        signal,
                                    },
                                );
                            }
                        })
                        .await;

                    match result {
                        Ok(response) => {
                            tracing::info!(
                                agent_id = %agent_id_owned,
                                stop_reason = %response.stop_reason,
                                "prompt completed"
                            );
                            send_push(
                                &sink,
                                FrontendEvent::Done {
                                    agent_id: agent_id_owned,
                                    stop_reason: response.stop_reason,
                                },
                            );
                        }
                        Err(err) => {
                            tracing::error!(?err, agent_id = %agent_id_owned, "prompt failed");
                            send_push(
                                &sink,
                                FrontendEvent::Error {
                                    agent_id: Some(agent_id_owned),
                                    message: err.to_string(),
                                },
                            );
                        }
                    }
                });
            }
            FrontendRequest::PermissionResponse {
                agent_id,
                id,
                granted,
            } => {
                tracing::info!(
                    agent_id = %agent_id,
                    permission_id = %id,
                    granted,
                    "received permission response"
                );

                let acp = match registry.get_acp_and_session(&agent_id).await {
                    Ok((acp, _, _, _)) => acp,
                    Err(err) => {
                        send_response(
                            &sink,
                            &request_id,
                            FrontendEvent::Error {
                                agent_id: Some(agent_id),
                                message: err.to_string(),
                            },
                        );
                        continue;
                    }
                };

                if let Err(err) = acp.respond_permission(&id, granted).await {
                    tracing::error!(?err, permission_id = %id, "failed to apply permission response");
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::Error {
                            agent_id: Some(agent_id),
                            message: err.to_string(),
                        },
                    );
                }
            }
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
                        observed == expected
                            || is_descendant_of_any(engine_pid, &[observed])
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
                            agent_id: None,
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
                            agent_id: None,
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
            FrontendRequest::ProbeRun {
                run_id,
                text,
            } => {
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
                            agent_id: None,
                            message: "probe_run requires app or Boss authority".to_owned(),
                        },
                    );
                    continue;
                }
                server_state.queue_probe(run_id.clone(), text);
                tracing::info!(run_id = %run_id, "probe queued");
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::ProbeQueued { run_id },
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
                            agent_id: None,
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
                send_response(
                    &sink,
                    &request_id,
                    FrontendEvent::RunStopped { run_id },
                );
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
                            agent_id: None,
                            message: "focus_worker_pane requires app or Boss authority"
                                .to_owned(),
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
                            agent_id: None,
                            message: "cancel_execution requires app or Boss authority"
                                .to_owned(),
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
            FrontendRequest::TailRunTranscript { run_id, lines } => {
                // `bossctl agents transcript` is a documented
                // coordinator verb. Same downgrade rationale as
                // `probe_run` and `stop_run`: BossOnly excluded worker
                // pane callers, so the coordinator couldn't tail a
                // sibling worker's transcript when running from inside
                // another worker. AppOrBoss admits worker descendants.
                if !server_state.authorize_rpc(RpcTier::AppOrBoss, peer_pid) {
                    tracing::warn!(
                        peer_pid = ?peer_pid,
                        run_id = %run_id,
                        "tail_run_transcript rejected: caller not in app/Boss subtree",
                    );
                    send_response(
                        &sink,
                        &request_id,
                        FrontendEvent::Error {
                            agent_id: None,
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
                            agent_id: None,
                            message: "workspace_pool_summary failed user-tier check"
                                .to_owned(),
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
                        let lease_to_execution = match server_state
                            .work_db
                            .lease_to_execution_map()
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
                        let workspaces = rows
                            .into_iter()
                            .map(|w| {
                                let execution_id = w
                                    .lease_id
                                    .as_ref()
                                    .and_then(|lease_id| {
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
        }
    }

    server_state.topic_broker.remove_session(&session_id).await;
    server_state.drop_app_session_if_matches(&session_id).await;
    sink.close();
    let _ = writer_task.await;
    Ok(())
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

async fn run_prompt(acp: &AcpClient, session_id: &str, prompt: &str) -> Result<()> {
    let response = acp
        .prompt_streaming(session_id, prompt, |event| match event {
            AcpEvent::AgentMessageChunk { text, .. } => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            AcpEvent::ToolCall { title, status, .. } => {
                eprintln!(
                    "\n[tool] {title} ({})",
                    status.unwrap_or_else(|| "started".to_owned())
                );
            }
            AcpEvent::ToolCallUpdate {
                tool_call_id,
                title,
                status,
                ..
            } => {
                let label =
                    title.unwrap_or_else(|| tool_call_id.unwrap_or_else(|| "tool".to_owned()));
                eprintln!(
                    "\n[tool-update] {label} ({})",
                    status.unwrap_or_else(|| "update".to_owned())
                );
            }
            AcpEvent::PermissionRequest { title, .. } => {
                eprintln!("\n[permission] auto-approving: {title}");
            }
            AcpEvent::TerminalStarted {
                title,
                command,
                cwd,
                ..
            } => {
                if let Some(cwd) = cwd {
                    eprintln!("\n[terminal] {title} (cwd={cwd})");
                } else {
                    eprintln!("\n[terminal] {title}");
                }
                eprintln!("{command}");
            }
            AcpEvent::TerminalOutput { text, .. } => {
                eprint!("{text}");
            }
            AcpEvent::TerminalDone {
                exit_code, signal, ..
            } => {
                if let Some(code) = exit_code {
                    eprintln!("\n[terminal done] exit={code}");
                } else if let Some(signal) = signal {
                    eprintln!("\n[terminal done] signal={signal}");
                } else {
                    eprintln!("\n[terminal done]");
                }
            }
        })
        .await?;

    eprintln!("\n[done] {}", response.stop_reason);
    Ok(())
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
        assert_eq!(
            q.enqueue(topic_envelope("b", 3)),
            EnqueueOutcome::Coalesced
        );
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
        assert_eq!(
            q.enqueue(response_envelope("r-1")),
            EnqueueOutcome::Closed
        );
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
        broker.subscribe("session-1", &["work.products".to_owned()]).await;

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
        server_state.drop_app_session_if_matches("session-app").await;

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
        server_state
            .worker_registry
            .register_run_slot("run-x", 1);
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
        let focus = tokio::spawn(async move {
            server_clone.focus_worker_pane("run-focus").await
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
        let focus = tokio::spawn(async move {
            server_clone.focus_worker_pane("run-focus").await
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
        // failure mode for AppOrBoss.
        let server_state = server_state_with_app_pid(1);
        server_state.set_boss_pid(2);
        let self_pid = std::process::id() as libc::pid_t;
        assert!(
            server_state.authorize_rpc(RpcTier::User, Some(self_pid)),
            "User tier must accept callers outside both trust subtrees",
        );
        assert!(
            !server_state.authorize_rpc(RpcTier::AppOrBoss, Some(self_pid)),
            "sanity: AppOrBoss must still reject the same caller, so the User-tier admission isn't an accidental hole",
        );
    }
}
