use serde::{Deserialize, Serialize};

use crate::engine_app::{EngineToAppRequest, EngineToAppResponse};
use crate::live_worker_state::LiveWorkerState;
use crate::types::{
    AddDependencyInput, CreateAttentionItemInput, CreateChoreInput, CreateExecutionInput,
    CreateProductInput, CreateProjectInput, CreateRunInput, CreateTaskInput,
    ListDependenciesInput, Product, Project, RemoveDependencyInput, RequestExecutionInput, Task,
    TaskRuntime, WorkAttentionItem, WorkExecution, WorkItem, WorkItemDependency,
    WorkItemDependencyView, WorkItemPatch, WorkRun,
};

pub const TOPIC_WORK_PRODUCTS: &str = "work.products";

pub fn work_product_topic(product_id: &str) -> String {
    format!("work.product.{product_id}")
}

pub fn execution_topic(execution_id: &str) -> String {
    format!("executions.{execution_id}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    #[default]
    Standard,
    Boss,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontendRequestEnvelope {
    pub request_id: String,
    pub payload: FrontendRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontendEventEnvelope {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    pub payload: FrontendEvent,
}

impl FrontendEventEnvelope {
    pub fn response(request_id: impl Into<String>, payload: FrontendEvent) -> Self {
        Self {
            request_id: Some(request_id.into()),
            revision: None,
            payload,
        }
    }

    pub fn push(payload: FrontendEvent) -> Self {
        Self {
            request_id: None,
            revision: None,
            payload,
        }
    }

    pub fn response_with_revision(
        request_id: impl Into<String>,
        revision: u64,
        payload: FrontendEvent,
    ) -> Self {
        Self {
            request_id: Some(request_id.into()),
            revision: Some(revision),
            payload,
        }
    }

    pub fn push_with_revision(revision: u64, payload: FrontendEvent) -> Self {
        Self {
            request_id: None,
            revision: Some(revision),
            payload,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FrontendRequest {
    Subscribe {
        topics: Vec<String>,
    },
    Unsubscribe {
        topics: Vec<String>,
    },
    CreateProduct {
        #[serde(flatten)]
        input: CreateProductInput,
    },
    ListProducts,
    ListProjects {
        product_id: String,
    },
    ListTasks {
        product_id: String,
        project_id: Option<String>,
    },
    ListChores {
        product_id: String,
    },
    GetWorkItem {
        id: String,
    },
    CreateProject {
        #[serde(flatten)]
        input: CreateProjectInput,
    },
    CreateTask {
        #[serde(flatten)]
        input: CreateTaskInput,
    },
    CreateChore {
        #[serde(flatten)]
        input: CreateChoreInput,
    },
    UpdateWorkItem {
        id: String,
        patch: WorkItemPatch,
    },
    DeleteWorkItem {
        id: String,
    },
    GetWorkTree {
        product_id: String,
    },
    ReorderProjectTasks {
        project_id: String,
        task_ids: Vec<String>,
    },
    CreateExecution {
        #[serde(flatten)]
        input: CreateExecutionInput,
    },
    RequestExecution {
        #[serde(flatten)]
        input: RequestExecutionInput,
    },
    ListExecutions {
        work_item_id: Option<String>,
    },
    GetExecution {
        id: String,
    },
    CreateRun {
        #[serde(flatten)]
        input: CreateRunInput,
    },
    ListRuns {
        execution_id: String,
    },
    GetRun {
        id: String,
    },
    CreateAttentionItem {
        #[serde(flatten)]
        input: CreateAttentionItemInput,
    },
    ListAttentionItems {
        execution_id: String,
    },
    GetAttentionItem {
        id: String,
    },
    CreateAgent {
        name: Option<String>,
        #[serde(default)]
        role: AgentRole,
    },
    ListAgents,
    RemoveAgent {
        agent_id: String,
    },
    Prompt {
        agent_id: String,
        text: String,
    },
    PermissionResponse {
        agent_id: String,
        id: String,
        granted: bool,
    },
    /// App self-identifies as the singleton app session. The engine
    /// rejects this unless `LOCAL_PEERPID` matches the app's pid (the
    /// engine's parent). After registration, `EngineRequest` events
    /// flow to this session only.
    RegisterAppSession,
    /// App tells the engine which pid is the Boss session's shell.
    /// Used to populate the second trust root for Boss-only RPCs.
    /// Only the registered app session may call this.
    RegisterBossSession {
        shell_pid: i32,
    },
    /// App's reply to a previous `FrontendEvent::EngineRequest`.
    /// `request_id` echoes the value the engine sent.
    EngineResponse {
        request_id: String,
        response: EngineToAppResponse,
    },
    /// Boss-tier RPC: queue a probe prompt for `run_id`. The engine
    /// holds the text until the next `Stop` hook event for that run,
    /// then writes it into the worker's pty as if it were typed by
    /// the user. Returns immediately with a `ProbeQueued` event;
    /// observation of the worker's reply is via the transcript.
    ProbeRun {
        run_id: String,
        text: String,
    },
    /// Boss-tier RPC: tear down the libghostty pane hosting `run_id`
    /// and release the cube workspace its execution still holds.
    /// Used by `bossctl agents stop`. Idempotent — duplicate requests
    /// (or one racing with completion-detection) collapse to a no-op
    /// on the second pass.
    StopRun {
        run_id: String,
    },
    /// Boss-tier RPC: bring the worker pane hosting `run_id` to the
    /// front in the macOS app. Resolves `run_id → slot_id` via the
    /// engine's worker registry and forwards a `FocusWorkerPane`
    /// engine→app request. Used by `bossctl agents focus`. Returns a
    /// `WorkError` if the run is unknown or has no allocated pane.
    FocusWorkerPane {
        run_id: String,
    },
    /// Boss-tier RPC: write `text` into the worker pane hosting
    /// `run_id` as if the user typed it. Resolves `run_id → slot_id`
    /// via the worker registry and forwards a `SendToPane` engine→app
    /// request, which the app routes through the same libghostty
    /// surface a real keystroke takes. Used by `bossctl agents send`.
    /// Returns `WorkError` if the run is unknown, has no allocated
    /// pane, or the app rejects the injection.
    SendInputToWorker {
        run_id: String,
        text: String,
    },
    /// Boss-tier RPC: interrupt the worker pane hosting `run_id` —
    /// equivalent to the human pressing Esc inside that pane.
    /// Resolves `run_id → slot_id` and forwards an
    /// `InterruptWorkerPane` engine→app request. Cancels the worker's
    /// in-flight turn without killing the run. Used by `bossctl
    /// agents interrupt`. Returns a `WorkError` if the run is unknown
    /// or has no allocated pane.
    InterruptWorkerPane {
        run_id: String,
    },
    /// Snapshot of every allocated worker slot's live state — what
    /// model it's running, what activity (working / waiting / idle /
    /// errored / terminated), most recent tool, etc. Source of truth
    /// for the kanban Doing-icon and the per-pane titlebar pill.
    /// Subscribers can also listen on the `worker.live_states` topic
    /// for push updates whenever any slot's state changes.
    ListWorkerLiveStates,
    /// Cancel a queued or running execution. Marks the execution row
    /// `cancelled`, releases any cube workspace lease it still holds,
    /// and tears down the libghostty pane (if one was allocated).
    /// Idempotent on already-terminal rows (returns `WorkError`).
    CancelExecution {
        execution_id: String,
    },
    /// Tail the most recent transcript chunk for `run_id`. The engine
    /// reads `WorkRun.transcript_path` and returns the trailing
    /// `lines` lines (raw JSONL — the caller decides how to render).
    /// Returns `WorkError` if the run is unknown or has no transcript
    /// path recorded yet.
    TailRunTranscript {
        run_id: String,
        lines: usize,
    },
    /// Snapshot the cube workspace pool. Proxies to
    /// `cube --json workspace list`; the engine adds no editorial — the
    /// returned vector mirrors cube's view, optionally annotated with
    /// the engine's own knowledge of which leases back which executions.
    WorkspacePoolSummary,
    /// Declare a `blocks` edge from `dependent` to `prerequisite`.
    /// Idempotent: re-adding an existing edge is a no-op. Cycles are
    /// rejected at the engine before insert.
    AddDependency {
        #[serde(flatten)]
        input: AddDependencyInput,
    },
    /// Drop the `(dependent, prerequisite, relation)` edge. No-op if
    /// the edge does not exist (mirrors `boss <kind> delete` on an
    /// already-archived row).
    RemoveDependency {
        #[serde(flatten)]
        input: RemoveDependencyInput,
    },
    /// Return the prerequisite and/or dependent edges for one work
    /// item. `direction` defaults to `both`.
    ListDependencies {
        #[serde(flatten)]
        input: ListDependenciesInput,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FrontendEvent {
    Hello {
        session_id: String,
    },
    Subscribed {
        topics: Vec<String>,
        current_revision: u64,
    },
    Unsubscribed {
        topics: Vec<String>,
    },
    TopicEvent {
        topic: String,
        revision: u64,
        origin_session_id: String,
        origin_request_id: Option<String>,
        event: TopicEventPayload,
    },
    ProductsList {
        products: Vec<Product>,
    },
    ProjectsList {
        product_id: String,
        projects: Vec<Project>,
    },
    TasksList {
        product_id: String,
        project_id: Option<String>,
        tasks: Vec<Task>,
    },
    ChoresList {
        product_id: String,
        chores: Vec<Task>,
    },
    WorkTree {
        product: Product,
        projects: Vec<Project>,
        tasks: Vec<Task>,
        chores: Vec<Task>,
        #[serde(default)]
        task_runtimes: Vec<TaskRuntime>,
    },
    WorkItemResult {
        item: WorkItem,
    },
    WorkItemCreated {
        item: WorkItem,
    },
    WorkItemUpdated {
        item: WorkItem,
    },
    ProjectTasksReordered {
        project_id: String,
        task_ids: Vec<String>,
    },
    ExecutionsList {
        work_item_id: Option<String>,
        executions: Vec<WorkExecution>,
    },
    ExecutionResult {
        execution: WorkExecution,
    },
    ExecutionCreated {
        execution: WorkExecution,
    },
    ExecutionRequested {
        execution: WorkExecution,
    },
    RunsList {
        execution_id: String,
        runs: Vec<WorkRun>,
    },
    RunResult {
        run: WorkRun,
    },
    RunCreated {
        run: WorkRun,
    },
    AttentionItemsList {
        execution_id: String,
        items: Vec<WorkAttentionItem>,
    },
    AttentionItemResult {
        item: WorkAttentionItem,
    },
    AttentionItemCreated {
        item: WorkAttentionItem,
    },
    WorkItemDeleted {
        id: String,
    },
    WorkError {
        message: String,
    },
    AgentCreated {
        agent_id: String,
        name: String,
        role: AgentRole,
    },
    AgentReady {
        agent_id: String,
    },
    AgentList {
        agents: Vec<AgentInfo>,
    },
    AgentRemoved {
        agent_id: String,
    },
    Chunk {
        agent_id: String,
        text: String,
    },
    Done {
        agent_id: String,
        stop_reason: String,
    },
    ToolCall {
        agent_id: String,
        name: String,
        status: String,
    },
    TerminalStarted {
        agent_id: String,
        id: String,
        title: String,
        command: String,
        cwd: Option<String>,
    },
    TerminalOutput {
        agent_id: String,
        id: String,
        text: String,
    },
    TerminalDone {
        agent_id: String,
        id: String,
        exit_code: Option<i64>,
        signal: Option<String>,
    },
    PermissionRequest {
        agent_id: String,
        id: String,
        title: String,
    },
    Error {
        agent_id: Option<String>,
        message: String,
    },
    /// Engine confirms the calling session is now the registered app
    /// session, and any prior registration was invalidated.
    AppSessionRegistered,
    /// Engine confirms the Boss session pid was registered.
    BossSessionRegistered,
    /// Engine confirms a probe was queued for the given run.
    ProbeQueued {
        run_id: String,
    },
    /// Engine acknowledges a stop request — the pane release has
    /// been kicked off and (if applicable) the cube workspace lease
    /// released. The reply does not wait for the libghostty pane to
    /// fully drain; teardown is asynchronous.
    RunStopped {
        run_id: String,
    },
    /// Engine acknowledges a focus request — the worker pane has
    /// been raised in the macOS app. Carries the resolved `slot_id`
    /// so the caller (e.g. `bossctl agents focus`) can confirm which
    /// slot was raised when the agent reference was a crew name or
    /// run id.
    WorkerPaneFocused {
        run_id: String,
        slot_id: u8,
    },
    /// Engine acknowledges a `SendInputToWorker` request — the text
    /// has been written into the worker pane via the same surface a
    /// user-typed keystroke takes. Carries the resolved `slot_id` so
    /// the caller (e.g. `bossctl agents send`) can confirm which
    /// pane was targeted when the agent reference was a crew name
    /// or run id.
    WorkerInputSent {
        run_id: String,
        slot_id: u8,
    },
    /// Engine acknowledges an interrupt request — an Esc keystroke
    /// has been delivered to the worker pane's pty. Carries the
    /// resolved `slot_id` so the caller can confirm which slot was
    /// interrupted when the agent reference was a crew name or run
    /// id.
    WorkerPaneInterrupted {
        run_id: String,
        slot_id: u8,
    },
    /// Engine asks the registered app session to perform a pane
    /// operation. The app must reply with a
    /// [`FrontendRequest::EngineResponse`] carrying the same
    /// `request_id`.
    EngineRequest {
        request_id: String,
        request: EngineToAppRequest,
    },
    /// Snapshot of every allocated worker slot's live state. Used as
    /// both the response to [`FrontendRequest::ListWorkerLiveStates`]
    /// and the body of pushes on the `worker.live_states` topic. The
    /// list is the entire snapshot, not a delta — receivers can
    /// blindly replace their local map.
    WorkerLiveStatesList {
        states: Vec<LiveWorkerState>,
    },
    /// Engine confirms an execution has been cancelled. The cancelled
    /// row's status is now `cancelled`; resource teardown (pane
    /// release, cube workspace release) is asynchronous.
    ExecutionCancelled {
        execution: WorkExecution,
    },
    /// Trailing transcript chunk for a run. `lines` are the raw JSONL
    /// lines the engine read off the recorded transcript path
    /// (newest-last). `truncated` is set when the file had more lines
    /// than were returned.
    RunTranscriptTail {
        run_id: String,
        transcript_path: String,
        lines: Vec<String>,
        truncated: bool,
    },
    /// Snapshot of the cube workspace pool. The engine proxies
    /// `cube --json workspace list`; each entry corresponds to one
    /// workspace cube knows about, annotated (when the engine has
    /// matching state) with the execution id currently leasing it.
    WorkspacePoolSummaryResult {
        workspaces: Vec<WorkspacePoolEntry>,
    },
    /// Engine confirms a dependency edge has been added. Returns the
    /// row that was inserted (or the existing row if the call was an
    /// idempotent re-add).
    DependencyAdded { edge: WorkItemDependency },
    /// Engine confirms a dependency edge has been removed (or that no
    /// matching edge existed to begin with — also a success).
    DependencyRemoved {
        dependent_id: String,
        prerequisite_id: String,
        relation: String,
        removed: bool,
    },
    /// Edge listing for a single work item, with prerequisites and
    /// dependents in two parallel lists.
    DependencyList { view: WorkItemDependencyView },
}

/// One row of the cube workspace pool, as exposed via
/// [`FrontendEvent::WorkspacePoolSummaryResult`]. Mirrors
/// `CubeWorkspaceStatus` plus an optional engine-side annotation
/// that maps a workspace's current lease to the execution holding it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspacePoolEntry {
    pub workspace_id: String,
    pub workspace_path: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leased_at_epoch_s: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_expires_at_epoch_s: Option<i64>,
    /// The execution id whose row currently records this lease, if
    /// the engine knows about one. Null when cube reports the lease
    /// but the engine has no matching execution row (drift) or the
    /// workspace is idle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub agent_id: String,
    pub name: String,
    #[serde(default)]
    pub role: AgentRole,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TopicEventPayload {
    WorkInvalidated {
        reason: String,
        product_id: Option<String>,
        item_ids: Vec<String>,
    },
    ExecutionInvalidated {
        reason: String,
        execution_id: String,
        work_item_id: String,
        status: String,
    },
}
