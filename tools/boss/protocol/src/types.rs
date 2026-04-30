use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Product {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub repo_remote_url: Option<String>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub product_id: String,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub goal: String,
    pub status: String,
    pub priority: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub product_id: String,
    pub project_id: Option<String>,
    pub kind: String,
    pub name: String,
    pub description: String,
    pub status: String,
    pub ordinal: Option<i64>,
    pub pr_url: Option<String>,
    pub deleted_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkExecution {
    pub id: String,
    pub work_item_id: String,
    pub kind: String,
    pub status: String,
    pub repo_remote_url: String,
    pub cube_repo_id: Option<String>,
    pub cube_lease_id: Option<String>,
    pub cube_workspace_id: Option<String>,
    pub workspace_path: Option<String>,
    #[serde(default)]
    pub priority: i64,
    pub preferred_workspace_id: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkRun {
    pub id: String,
    pub execution_id: String,
    pub agent_id: String,
    pub status: String,
    pub error_text: Option<String>,
    pub result_summary: Option<String>,
    pub transcript_path: Option<String>,
    pub artifacts_path: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkAttentionItem {
    pub id: String,
    pub execution_id: String,
    pub kind: String,
    pub status: String,
    pub title: String,
    pub body_markdown: String,
    pub created_at: String,
    pub resolved_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionReconcileResult {
    pub created: Vec<WorkExecution>,
    pub updated: Vec<WorkExecution>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkTree {
    pub product: Product,
    pub projects: Vec<Project>,
    pub tasks: Vec<Task>,
    pub chores: Vec<Task>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "item_type", rename_all = "snake_case")]
pub enum WorkItem {
    Product(Product),
    Project(Project),
    Task(Task),
    Chore(Task),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProductInput {
    pub name: String,
    pub description: Option<String>,
    pub repo_remote_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProjectInput {
    pub product_id: String,
    pub name: String,
    pub description: Option<String>,
    pub goal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTaskInput {
    pub product_id: String,
    pub project_id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateChoreInput {
    pub product_id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateExecutionInput {
    pub work_item_id: String,
    pub kind: String,
    pub status: Option<String>,
    pub repo_remote_url: Option<String>,
    pub cube_repo_id: Option<String>,
    pub cube_lease_id: Option<String>,
    pub cube_workspace_id: Option<String>,
    pub workspace_path: Option<String>,
    pub priority: Option<i64>,
    pub preferred_workspace_id: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestExecutionInput {
    pub work_item_id: String,
    pub priority: Option<i64>,
    pub preferred_workspace_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateRunInput {
    pub execution_id: String,
    pub agent_id: String,
    pub status: Option<String>,
    pub error_text: Option<String>,
    pub result_summary: Option<String>,
    pub transcript_path: Option<String>,
    pub artifacts_path: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateAttentionItemInput {
    pub execution_id: String,
    pub kind: String,
    pub status: Option<String>,
    pub title: String,
    pub body_markdown: String,
    pub resolved_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkItemPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub goal: Option<String>,
    pub priority: Option<String>,
    pub repo_remote_url: Option<String>,
    pub pr_url: Option<String>,
    pub ordinal: Option<i64>,
}
