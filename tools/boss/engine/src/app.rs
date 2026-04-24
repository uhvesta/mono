use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc};

use crate::acp::{AcpClient, AcpEvent};
use crate::cli::{Cli, Mode};
use crate::config::RuntimeConfig;
use crate::coordinator::{CommandCubeClient, ExecutionCoordinator, WorkerPool};
use crate::protocol::{
    AgentInfo, AgentRole, FrontendEvent, FrontendEventEnvelope, FrontendRequest,
    FrontendRequestEnvelope, TOPIC_WORK_PRODUCTS, TopicEventPayload, work_product_topic,
};
use crate::runner::AcpExecutionRunner;
use crate::work::{WorkDb, WorkItem};

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

Default behavior:
- clarify goals and scope,
- queue likely work immediately, including investigation work,
- ask only when you cannot reasonably infer the destination product or representation,
- use the current product and existing project context before choosing task, chore, or project,
- avoid repo inspection and detailed technical analysis before the work is queued,
- keep status and structure accurate,
- suggest or assign implementation and investigation work rather than doing it yourself."#;

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
    cfg: RuntimeConfig,
}

impl AgentRegistry {
    fn new(cfg: RuntimeConfig) -> Self {
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
        let session_id = acp_client.new_session(&self.cfg.cwd).await?;
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
    topic_broker: Arc<TopicBroker>,
    next_session_id: AtomicU64,
    work_revision: AtomicU64,
}

impl ServerState {
    fn new(cfg: &RuntimeConfig) -> Result<Self> {
        let work_db = Arc::new(WorkDb::open(cfg.db_path.clone())?);
        let worker_pool = WorkerPool::new(cfg.worker_pool_size);
        let execution_coordinator = Arc::new(ExecutionCoordinator::new(
            work_db.clone(),
            worker_pool,
            Arc::new(CommandCubeClient::new(cfg.clone())),
            Arc::new(AcpExecutionRunner::new(cfg.clone())),
        ));
        Ok(Self {
            work_db,
            agent_registry: Arc::new(AgentRegistry::new(cfg.clone())),
            execution_coordinator,
            topic_broker: Arc::new(TopicBroker::default()),
            next_session_id: AtomicU64::new(1),
            work_revision: AtomicU64::new(0),
        })
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

#[derive(Default)]
struct TopicBroker {
    inner: Mutex<TopicBrokerInner>,
}

#[derive(Default)]
struct TopicBrokerInner {
    senders: HashMap<String, mpsc::UnboundedSender<FrontendEventEnvelope>>,
    topics_by_session: HashMap<String, HashSet<String>>,
    sessions_by_topic: HashMap<String, HashSet<String>>,
}

impl TopicBroker {
    async fn register_session(
        &self,
        session_id: &str,
        sender: mpsc::UnboundedSender<FrontendEventEnvelope>,
    ) {
        let mut inner = self.inner.lock().await;
        inner.senders.insert(session_id.to_owned(), sender);
    }

    async fn remove_session(&self, session_id: &str) {
        let mut inner = self.inner.lock().await;
        inner.senders.remove(session_id);
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

    #[allow(dead_code)]
    async fn publish(&self, topic: &str, envelope: FrontendEventEnvelope) {
        let senders = {
            let inner = self.inner.lock().await;
            inner
                .sessions_by_topic
                .get(topic)
                .into_iter()
                .flat_map(|sessions| sessions.iter())
                .filter_map(|session_id| inner.senders.get(session_id).cloned())
                .collect::<Vec<_>>()
        };

        for sender in senders {
            let _ = sender.send(envelope.clone());
        }
    }
}

pub async fn run(cli: Cli) -> Result<()> {
    let cfg = RuntimeConfig::load_from_env()?;
    tracing::info!(
        acp_command = %cfg.acp.command,
        acp_args = ?cfg.acp.args,
        cwd = %cfg.cwd.display(),
        db_path = %cfg.db_path.display(),
        "starting boss-engine runtime",
    );

    match cli.mode {
        Mode::Cli => run_cli(cli, &cfg).await,
        Mode::Server => run_server(cli, &cfg).await,
    }
}

async fn run_cli(cli: Cli, cfg: &RuntimeConfig) -> Result<()> {
    cfg.preflight_acp()?;
    let acp = AcpClient::connect(cfg).await?;
    acp.initialize().await?;
    let session_id = acp.new_session(&cfg.cwd).await?;

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

async fn run_server(cli: Cli, cfg: &RuntimeConfig) -> Result<()> {
    let server_state = Arc::new(ServerState::new(cfg)?);
    let socket_path = cli
        .socket_path
        .unwrap_or_else(|| DEFAULT_SOCKET_PATH.to_owned());

    if Path::new(&socket_path).exists() {
        tokio::fs::remove_file(&socket_path)
            .await
            .with_context(|| format!("failed to remove existing socket {socket_path}"))?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind unix socket {socket_path}"))?;

    let pid_path =
        std::env::var("BOSS_ENGINE_PID_PATH").unwrap_or_else(|_| DEFAULT_PID_PATH.to_owned());
    let pid = std::process::id();
    std::fs::write(&pid_path, format!("{pid}\n"))
        .with_context(|| format!("failed to write pid file {pid_path}"))?;
    let _pid_guard = PidFileGuard {
        path: pid_path.clone(),
        pid,
    };

    tracing::info!(socket_path = %socket_path, "frontend socket is ready");
    tracing::info!(pid, pid_file = %pid_path, "engine pid file is ready");
    println!("boss-engine listening on {socket_path}");

    let coordinator = server_state.execution_coordinator.clone();
    coordinator.kick();

    loop {
        let (stream, _) = listener.accept().await.context("socket accept failed")?;
        let cfg = cfg.clone();
        let server_state = server_state.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_frontend_connection(stream, &cfg, server_state).await {
                tracing::error!(?err, "frontend connection failed");
            }
        });
    }
}

async fn handle_frontend_connection(
    stream: UnixStream,
    _cfg: &RuntimeConfig,
    server_state: Arc<ServerState>,
) -> Result<()> {
    tracing::info!("frontend connected");
    let registry = server_state.agent_registry.clone();
    let work_db = server_state.work_db.clone();
    let session_id = server_state.allocate_session_id();

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half).lines();

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<FrontendEventEnvelope>();
    server_state
        .topic_broker
        .register_session(&session_id, event_tx.clone())
        .await;
    let _ = event_tx.send(FrontendEventEnvelope::push(FrontendEvent::Hello {
        session_id: session_id.clone(),
    }));

    let writer_task = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
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
    });

    while let Some(line) = reader.next_line().await.context("socket read failed")? {
        if line.trim().is_empty() {
            continue;
        }

        let envelope: FrontendRequestEnvelope = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(err) => {
                let _ = event_tx.send(FrontendEventEnvelope::push(FrontendEvent::Error {
                    agent_id: None,
                    message: format!("invalid request payload: {err}"),
                }));
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
                    &event_tx,
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
                    &event_tx,
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
                        &event_tx,
                        &request_id,
                        revision,
                        FrontendEvent::WorkItemCreated { item },
                    );
                }
                Err(err) => {
                    send_response(
                        &event_tx,
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
                        &event_tx,
                        &request_id,
                        server_state.current_work_revision(),
                        FrontendEvent::ProductsList { products },
                    );
                }
                Err(err) => {
                    send_response(
                        &event_tx,
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
                            &event_tx,
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
                            &event_tx,
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
                        &event_tx,
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
                        &event_tx,
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
                        &event_tx,
                        &request_id,
                        server_state.current_work_revision(),
                        FrontendEvent::ChoresList { product_id, chores },
                    );
                }
                Err(err) => {
                    send_response(
                        &event_tx,
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
                        &event_tx,
                        &request_id,
                        server_state.current_work_revision(),
                        FrontendEvent::WorkItemResult { item },
                    );
                }
                Err(err) => {
                    send_response(
                        &event_tx,
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
                        &event_tx,
                        &request_id,
                        revision,
                        FrontendEvent::WorkItemCreated { item },
                    );
                }
                Err(err) => {
                    send_response(
                        &event_tx,
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
                        &event_tx,
                        &request_id,
                        revision,
                        FrontendEvent::WorkItemCreated { item },
                    );
                }
                Err(err) => {
                    send_response(
                        &event_tx,
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
                        &event_tx,
                        &request_id,
                        revision,
                        FrontendEvent::WorkItemCreated { item },
                    );
                }
                Err(err) => {
                    send_response(
                        &event_tx,
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
                            &event_tx,
                            &request_id,
                            revision,
                            FrontendEvent::WorkItemUpdated { item },
                        );
                    }
                    Err(err) => {
                        send_response(
                            &event_tx,
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
                            &event_tx,
                            &request_id,
                            revision,
                            FrontendEvent::WorkItemDeleted { id },
                        );
                    }
                    Err(err) => {
                        send_response(
                            &event_tx,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: err.to_string(),
                            },
                        );
                    }
                },
                Err(err) => {
                    send_response(
                        &event_tx,
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
                        &event_tx,
                        &request_id,
                        server_state.current_work_revision(),
                        FrontendEvent::WorkTree {
                            product: tree.product,
                            projects: tree.projects,
                            tasks: tree.tasks,
                            chores: tree.chores,
                        },
                    );
                }
                Err(err) => {
                    send_response(
                        &event_tx,
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
                            &event_tx,
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
                            &event_tx,
                            &request_id,
                            FrontendEvent::WorkError {
                                message: err.to_string(),
                            },
                        );
                    }
                },
                Err(err) => {
                    send_response(
                        &event_tx,
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
                        &event_tx,
                        &request_id,
                        FrontendEvent::ExecutionCreated { execution },
                    );
                }
                Err(err) => {
                    send_response(
                        &event_tx,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: err.to_string(),
                        },
                    );
                }
            },
            FrontendRequest::ListExecutions { work_item_id } => {
                match work_db.list_executions(work_item_id.as_deref()) {
                    Ok(executions) => {
                        send_response(
                            &event_tx,
                            &request_id,
                            FrontendEvent::ExecutionsList {
                                work_item_id,
                                executions,
                            },
                        );
                    }
                    Err(err) => {
                        send_response(
                            &event_tx,
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
                        &event_tx,
                        &request_id,
                        FrontendEvent::ExecutionResult { execution },
                    );
                }
                Err(err) => {
                    send_response(
                        &event_tx,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: err.to_string(),
                        },
                    );
                }
            },
            FrontendRequest::CreateRun { input } => match work_db.create_run(input) {
                Ok(run) => {
                    send_response(&event_tx, &request_id, FrontendEvent::RunCreated { run });
                }
                Err(err) => {
                    send_response(
                        &event_tx,
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
                        &event_tx,
                        &request_id,
                        FrontendEvent::RunsList { execution_id, runs },
                    );
                }
                Err(err) => {
                    send_response(
                        &event_tx,
                        &request_id,
                        FrontendEvent::WorkError {
                            message: err.to_string(),
                        },
                    );
                }
            },
            FrontendRequest::GetRun { id } => match work_db.get_run(&id) {
                Ok(run) => {
                    send_response(&event_tx, &request_id, FrontendEvent::RunResult { run });
                }
                Err(err) => {
                    send_response(
                        &event_tx,
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
                            &event_tx,
                            &request_id,
                            FrontendEvent::AttentionItemCreated { item },
                        );
                    }
                    Err(err) => {
                        send_response(
                            &event_tx,
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
                            &event_tx,
                            &request_id,
                            FrontendEvent::AttentionItemsList {
                                execution_id,
                                items,
                            },
                        );
                    }
                    Err(err) => {
                        send_response(
                            &event_tx,
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
                        &event_tx,
                        &request_id,
                        FrontendEvent::AttentionItemResult { item },
                    );
                }
                Err(err) => {
                    send_response(
                        &event_tx,
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
                    &event_tx,
                    &request_id,
                    FrontendEvent::AgentCreated {
                        agent_id: agent_id.clone(),
                        name: agent_name.clone(),
                        role,
                    },
                );

                let event_tx = event_tx.clone();
                let registry = registry.clone();
                tokio::spawn(async move {
                    match registry
                        .initialize_agent(&agent_id, &agent_name, role)
                        .await
                    {
                        Ok(()) => {
                            send_push(&event_tx, FrontendEvent::AgentReady { agent_id });
                        }
                        Err(err) => {
                            tracing::error!(?err, agent_id = %agent_id, "failed to initialize agent");
                            send_push(
                                &event_tx,
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
                send_response(&event_tx, &request_id, FrontendEvent::AgentList { agents });
            }
            FrontendRequest::RemoveAgent { agent_id } => {
                match registry.remove_agent(&agent_id).await {
                    Ok(()) => {
                        send_response(
                            &event_tx,
                            &request_id,
                            FrontendEvent::AgentRemoved { agent_id },
                        );
                    }
                    Err(err) => {
                        tracing::error!(?err, agent_id = %agent_id, "failed to remove agent");
                        send_response(
                            &event_tx,
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
                                &event_tx,
                                &request_id,
                                FrontendEvent::Error {
                                    agent_id: Some(agent_id),
                                    message: err.to_string(),
                                },
                            );
                            continue;
                        }
                    };

                let event_tx = event_tx.clone();
                let agent_id_owned = agent_id.clone();
                let prompt_text = compose_agent_prompt(system_prompt.as_deref(), &text);

                tokio::spawn(async move {
                    let _guard = prompt_lock.lock().await;
                    let aid = agent_id_owned.clone();

                    let result = acp
                        .prompt_streaming(&session_id, &prompt_text, |event| match event {
                            AcpEvent::AgentMessageChunk { text, .. } => {
                                send_push(
                                    &event_tx,
                                    FrontendEvent::Chunk {
                                        agent_id: aid.clone(),
                                        text,
                                    },
                                );
                            }
                            AcpEvent::ToolCall { title, status, .. } => {
                                send_push(
                                    &event_tx,
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
                                    &event_tx,
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
                                    &event_tx,
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
                                    &event_tx,
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
                                    &event_tx,
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
                                    &event_tx,
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
                                &event_tx,
                                FrontendEvent::Done {
                                    agent_id: agent_id_owned,
                                    stop_reason: response.stop_reason,
                                },
                            );
                        }
                        Err(err) => {
                            tracing::error!(?err, agent_id = %agent_id_owned, "prompt failed");
                            send_push(
                                &event_tx,
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
                            &event_tx,
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
                        &event_tx,
                        &request_id,
                        FrontendEvent::Error {
                            agent_id: Some(agent_id),
                            message: err.to_string(),
                        },
                    );
                }
            }
        }
    }

    server_state.topic_broker.remove_session(&session_id).await;
    drop(event_tx);
    let _ = writer_task.await;
    Ok(())
}

fn send_response(
    event_tx: &mpsc::UnboundedSender<FrontendEventEnvelope>,
    request_id: &str,
    payload: FrontendEvent,
) {
    let _ = event_tx.send(FrontendEventEnvelope::response(
        request_id.to_owned(),
        payload,
    ));
}

fn send_response_with_revision(
    event_tx: &mpsc::UnboundedSender<FrontendEventEnvelope>,
    request_id: &str,
    revision: u64,
    payload: FrontendEvent,
) {
    let _ = event_tx.send(FrontendEventEnvelope::response_with_revision(
        request_id.to_owned(),
        revision,
        payload,
    ));
}

fn send_push(event_tx: &mpsc::UnboundedSender<FrontendEventEnvelope>, payload: FrontendEvent) {
    let _ = event_tx.send(FrontendEventEnvelope::push(payload));
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
