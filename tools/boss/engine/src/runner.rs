use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::acp::{AcpClient, AcpEvent};
use crate::config::RuntimeConfig;
use crate::work::{Project, Task, WorkExecution, WorkItem};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAttention {
    pub kind: String,
    pub title: String,
    pub body_markdown: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    pub execution_status: String,
    pub result_summary: Option<String>,
    pub attention: Option<RunAttention>,
    pub release_workspace: bool,
}

#[async_trait]
pub trait ExecutionRunner: Send + Sync {
    async fn run_execution(
        &self,
        worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace_path: &Path,
    ) -> Result<RunOutcome>;
}

pub struct AcpExecutionRunner {
    cfg: RuntimeConfig,
    workers: Mutex<HashMap<String, Arc<WorkerClient>>>,
}

struct WorkerClient {
    acp: Arc<AcpClient>,
    prompt_lock: Mutex<()>,
}

impl AcpExecutionRunner {
    pub fn new(cfg: RuntimeConfig) -> Self {
        Self {
            cfg,
            workers: Mutex::new(HashMap::new()),
        }
    }

    async fn worker_client(&self, worker_id: &str) -> Result<Arc<WorkerClient>> {
        if let Some(worker) = self.workers.lock().await.get(worker_id).cloned() {
            return Ok(worker);
        }

        let acp = Arc::new(AcpClient::connect(&self.cfg).await?);
        acp.initialize().await?;
        let worker = Arc::new(WorkerClient {
            acp,
            prompt_lock: Mutex::new(()),
        });

        let mut workers = self.workers.lock().await;
        Ok(workers
            .entry(worker_id.to_owned())
            .or_insert_with(|| worker.clone())
            .clone())
    }
}

#[async_trait]
impl ExecutionRunner for AcpExecutionRunner {
    async fn run_execution(
        &self,
        worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace_path: &Path,
    ) -> Result<RunOutcome> {
        let worker = self.worker_client(worker_id).await?;
        let _guard = worker.prompt_lock.lock().await;
        let session_id = worker.acp.new_session(workspace_path).await?;
        let prompt = compose_execution_prompt(execution, work_item, workspace_path);
        let mut transcript = String::new();

        let response = worker
            .acp
            .prompt_streaming(&session_id, &prompt, |event| match event {
                AcpEvent::AgentMessageChunk { text, .. } => {
                    transcript.push_str(&text);
                }
                AcpEvent::ToolCall { title, status, .. } => {
                    tracing::info!(
                        worker_id,
                        execution_id = %execution.id,
                        tool = %title,
                        status = ?status,
                        "execution worker tool call"
                    );
                }
                AcpEvent::ToolCallUpdate {
                    tool_call_id,
                    title,
                    status,
                    ..
                } => {
                    tracing::info!(
                        worker_id,
                        execution_id = %execution.id,
                        tool_call_id = ?tool_call_id,
                        title = ?title,
                        status = ?status,
                        "execution worker tool update"
                    );
                }
                AcpEvent::PermissionRequest { title, .. } => {
                    tracing::warn!(
                        worker_id,
                        execution_id = %execution.id,
                        title,
                        "execution worker requested interactive permission"
                    );
                }
                AcpEvent::TerminalStarted {
                    id,
                    title,
                    command,
                    cwd,
                    ..
                } => {
                    tracing::info!(
                        worker_id,
                        execution_id = %execution.id,
                        terminal_id = %id,
                        title,
                        command,
                        cwd = ?cwd,
                        "execution worker terminal started"
                    );
                }
                AcpEvent::TerminalOutput { .. } => {}
                AcpEvent::TerminalDone {
                    id,
                    exit_code,
                    signal,
                    ..
                } => {
                    tracing::info!(
                        worker_id,
                        execution_id = %execution.id,
                        terminal_id = %id,
                        exit_code = ?exit_code,
                        signal = ?signal,
                        "execution worker terminal finished"
                    );
                }
            })
            .await?;

        let result_summary = summarize_run_output(&transcript, &response.stop_reason);
        let attention = Some(review_attention(
            execution,
            work_item,
            workspace_path,
            result_summary.as_deref(),
        ));

        Ok(RunOutcome {
            execution_status: "waiting_human".to_owned(),
            result_summary,
            attention,
            release_workspace: false,
        })
    }
}

fn compose_execution_prompt(
    execution: &WorkExecution,
    work_item: &WorkItem,
    workspace_path: &Path,
) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are a reusable Boss worker running one execution inside a dedicated repo workspace.\n",
    );
    prompt.push_str("The current session cwd is already set to that workspace.\n");
    prompt.push_str("Do the work directly in the repository checkout before ending this run.\n");
    prompt.push_str("Avoid asking the human for permission during this pass; when you need review or direction, stop and summarize it clearly.\n\n");
    prompt.push_str("Execution context:\n");
    prompt.push_str(&format!("- execution id: `{}`\n", execution.id));
    prompt.push_str(&format!("- execution kind: `{}`\n", execution.kind));
    prompt.push_str(&format!("- workspace: `{}`\n", workspace_path.display()));
    prompt.push_str(&format!("- work item: `{}`\n", work_item_name(work_item)));
    if let Some(details) = work_item_details(work_item) {
        prompt.push_str("- details:\n");
        prompt.push_str(details.trim_end());
        prompt.push('\n');
    }
    prompt.push('\n');
    prompt.push_str(match execution.kind.as_str() {
        "project_design" => {
            "Expected outcome for this run:\n- inspect the repository and relevant context,\n- draft or update a repo-backed design artifact,\n- identify likely follow-up tasks or phases,\n- stop once the design pass is in a state a human can review.\n"
        }
        "task_implementation" | "chore_implementation" => {
            "Expected outcome for this run:\n- implement the requested change in the workspace,\n- run relevant local validation when practical,\n- stop once the work is ready for a human to review or redirect.\n"
        }
        _ => {
            "Expected outcome for this run:\n- make concrete progress on the assigned work,\n- leave the workspace in a reviewable state,\n- stop with a concise review summary.\n"
        }
    });
    prompt.push_str("\nRespond with concise markdown using exactly these sections:\n");
    prompt.push_str("## Summary\n## Validation\n## Open Questions\n");
    prompt
}

fn work_item_name(work_item: &WorkItem) -> &str {
    match work_item {
        WorkItem::Product(product) => &product.name,
        WorkItem::Project(project) => &project.name,
        WorkItem::Task(task) | WorkItem::Chore(task) => &task.name,
    }
}

fn work_item_details(work_item: &WorkItem) -> Option<String> {
    match work_item {
        WorkItem::Product(product) => {
            if product.description.trim().is_empty() {
                None
            } else {
                Some(format!("  - description: {}", product.description.trim()))
            }
        }
        WorkItem::Project(project) => project_details(project),
        WorkItem::Task(task) | WorkItem::Chore(task) => task_details(task),
    }
}

fn project_details(project: &Project) -> Option<String> {
    let mut lines = Vec::new();
    if !project.description.trim().is_empty() {
        lines.push(format!("  - description: {}", project.description.trim()));
    }
    if !project.goal.trim().is_empty() {
        lines.push(format!("  - goal: {}", project.goal.trim()));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn task_details(task: &Task) -> Option<String> {
    let mut lines = Vec::new();
    if !task.description.trim().is_empty() {
        lines.push(format!("  - description: {}", task.description.trim()));
    }
    if let Some(pr_url) = task.pr_url.as_deref() {
        if !pr_url.trim().is_empty() {
            lines.push(format!("  - pr_url: {}", pr_url.trim()));
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn summarize_run_output(transcript: &str, stop_reason: &str) -> Option<String> {
    let trimmed = transcript.trim();
    if trimmed.is_empty() {
        return Some(format!(
            "Worker run ended with stop reason `{stop_reason}` and did not return a textual summary."
        ));
    }

    Some(truncate_chars(trimmed, 4000))
}

fn review_attention(
    execution: &WorkExecution,
    work_item: &WorkItem,
    workspace_path: &Path,
    result_summary: Option<&str>,
) -> RunAttention {
    let title = match execution.kind.as_str() {
        "project_design" => format!("Review design output for {}", work_item_name(work_item)),
        _ => format!(
            "Review implementation output for {}",
            work_item_name(work_item)
        ),
    };

    let summary = result_summary.unwrap_or("_No summary was captured for this run._");
    let body_markdown = format!(
        "Execution `{}` is waiting for human review.\n\n- work item: `{}`\n- execution kind: `{}`\n- workspace: `{}`\n\n## Run Summary\n{}\n",
        execution.id,
        work_item_name(work_item),
        execution.kind,
        workspace_path.display(),
        summary,
    );

    RunAttention {
        kind: "review_required".to_owned(),
        title,
        body_markdown,
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    let mut truncated = String::new();
    for (count, ch) in text.chars().enumerate() {
        if count >= limit {
            truncated.push_str("\n\n...[truncated]");
            return truncated;
        }
        truncated.push(ch);
    }
    truncated
}
