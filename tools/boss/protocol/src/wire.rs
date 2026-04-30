use serde::{Deserialize, Serialize};

use crate::types::{
    CreateAttentionItemInput, CreateChoreInput, CreateExecutionInput, CreateProductInput,
    CreateProjectInput, CreateRunInput, CreateTaskInput, Product, Project,
    RequestExecutionInput, Task, WorkAttentionItem, WorkExecution, WorkItem, WorkItemPatch,
    WorkRun,
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
