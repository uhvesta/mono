use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};

pub use boss_protocol::{
    CreateAttentionItemInput, CreateChoreInput, CreateExecutionInput, CreateProductInput,
    CreateProjectInput, CreateRunInput, CreateTaskInput, ExecutionReconcileResult, Product,
    Project, RequestExecutionInput, Task, WorkAttentionItem, WorkExecution, WorkItem,
    WorkItemPatch, WorkRun, WorkTree,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct WorkDb {
    path: PathBuf,
}

impl WorkDb {
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create work db directory {}", parent.display())
            })?;
        }

        let db = Self { path };
        db.init()?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn list_products(&self) -> Result<Vec<Product>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, slug, description, repo_remote_url, status, created_at, updated_at
             FROM products
             ORDER BY name COLLATE NOCASE ASC",
        )?;
        let rows = stmt.query_map([], map_product)?;
        collect_rows(rows)
    }

    pub fn create_product(&self, input: CreateProductInput) -> Result<Product> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;

        let id = next_id("prod");
        let now = now_string();
        let slug = unique_product_slug(&tx, &slugify(&input.name))?;
        let description = input.description.unwrap_or_default();
        let repo_remote_url = normalize_optional_text(input.repo_remote_url);

        tx.execute(
            "INSERT INTO products (id, name, slug, description, repo_remote_url, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, ?6)",
            params![id, input.name, slug, description, repo_remote_url, now],
        )?;

        let product = query_product(&tx, &id)?
            .with_context(|| format!("missing product after insert: {id}"))?;
        tx.commit()?;
        Ok(product)
    }

    pub fn list_projects(&self, product_id: &str) -> Result<Vec<Project>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at
             FROM projects
             WHERE product_id = ?1
             ORDER BY created_at ASC, name COLLATE NOCASE ASC",
        )?;
        let rows = stmt.query_map([product_id], map_project)?;
        collect_rows(rows)
    }

    pub fn create_project(&self, input: CreateProjectInput) -> Result<Project> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_product_exists(&tx, &input.product_id)?;

        let id = next_id("proj");
        let now = now_string();
        let slug = unique_project_slug(&tx, &input.product_id, &slugify(&input.name))?;
        let description = input.description.unwrap_or_default();
        let goal = input.goal.unwrap_or_default();

        tx.execute(
            "INSERT INTO projects (id, product_id, name, slug, description, goal, status, priority, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'planned', 'medium', ?7, ?7)",
            params![id, input.product_id, input.name, slug, description, goal, now],
        )?;

        let project = query_project(&tx, &id)?
            .with_context(|| format!("missing project after insert: {id}"))?;
        tx.commit()?;
        Ok(project)
    }

    pub fn create_task(&self, input: CreateTaskInput) -> Result<Task> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_product_exists(&tx, &input.product_id)?;
        ensure_project_belongs_to_product(&tx, &input.project_id, &input.product_id)?;

        let id = next_id("task");
        let now = now_string();
        let ordinal = next_task_ordinal(&tx, &input.project_id)?;
        let description = input.description.unwrap_or_default();

        tx.execute(
            "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'project_task', ?4, ?5, 'todo', ?6, NULL, NULL, ?7, ?7)",
            params![id, input.product_id, input.project_id, input.name, description, ordinal, now],
        )?;

        let task =
            query_task(&tx, &id)?.with_context(|| format!("missing task after insert: {id}"))?;
        tx.commit()?;
        Ok(task)
    }

    pub fn create_chore(&self, input: CreateChoreInput) -> Result<Task> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_product_exists(&tx, &input.product_id)?;

        let id = next_id("task");
        let now = now_string();
        let description = input.description.unwrap_or_default();

        tx.execute(
            "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at)
             VALUES (?1, ?2, NULL, 'chore', ?3, ?4, 'todo', NULL, NULL, NULL, ?5, ?5)",
            params![id, input.product_id, input.name, description, now],
        )?;

        let task =
            query_task(&tx, &id)?.with_context(|| format!("missing chore after insert: {id}"))?;
        tx.commit()?;
        Ok(task)
    }

    pub fn create_execution(&self, input: CreateExecutionInput) -> Result<WorkExecution> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = insert_execution(&tx, input)?;
        tx.commit()?;
        Ok(execution)
    }

    /// Returns or creates a ready execution for `work_item_id`, applying any
    /// priority / preferred-workspace overrides from the request.
    ///
    /// If the most recent execution for this work item is still in flight
    /// (`ready` / `running` / `waiting_*`) we update its priority and
    /// preferred_workspace_id rather than creating a duplicate. If it is
    /// terminal (or absent), we insert a fresh `ready` execution.
    pub fn request_execution(
        &self,
        input: RequestExecutionInput,
    ) -> Result<WorkExecution> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = request_execution_in_tx(&tx, input)?;
        tx.commit()?;
        Ok(execution)
    }

    pub fn list_executions(&self, work_item_id: Option<&str>) -> Result<Vec<WorkExecution>> {
        let conn = self.connect()?;
        if let Some(work_item_id) = work_item_id {
            let _ = product_id_for_work_item(&conn, work_item_id)?;
            let mut stmt = conn.prepare(
                "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                        cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                        created_at, started_at, finished_at
                 FROM work_executions
                 WHERE work_item_id = ?1
                 ORDER BY created_at ASC, id ASC",
            )?;
            let rows = stmt.query_map([work_item_id], map_execution)?;
            return collect_rows(rows);
        }

        let mut stmt = conn.prepare(
            "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                    cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                    created_at, started_at, finished_at
             FROM work_executions
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], map_execution)?;
        collect_rows(rows)
    }

    pub fn get_execution(&self, id: &str) -> Result<WorkExecution> {
        let conn = self.connect()?;
        query_execution(&conn, id)?.with_context(|| format!("unknown execution: {id}"))
    }

    pub fn list_ready_executions(&self) -> Result<Vec<WorkExecution>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                    cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                    created_at, started_at, finished_at
             FROM work_executions
             WHERE status = 'ready'
             ORDER BY priority DESC, created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], map_execution)?;
        collect_rows(rows)
    }

    pub fn reconcile_product_executions(
        &self,
        product_id: &str,
    ) -> Result<ExecutionReconcileResult> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let product = query_product(&tx, product_id)?
            .with_context(|| format!("unknown product: {product_id}"))?;
        let projects = list_projects_for_product(&tx, product_id)?;
        let tasks = list_tasks_for_product(&tx, product_id)?;
        let mut result = ExecutionReconcileResult::default();

        let repo_remote_url = product.repo_remote_url.clone();

        for project in &projects {
            if !project_accepts_execution(project) {
                continue;
            }
            reconcile_work_item_execution(
                &tx,
                &mut result,
                &project.id,
                "project_design",
                "ready",
                repo_remote_url.as_deref(),
            )?;
        }

        let mut project_tasks: HashMap<String, Vec<Task>> = HashMap::new();
        for task in tasks {
            match task.kind.as_str() {
                "chore" => {
                    if task_accepts_execution(&task) {
                        reconcile_work_item_execution(
                            &tx,
                            &mut result,
                            &task.id,
                            "chore_implementation",
                            "ready",
                            repo_remote_url.as_deref(),
                        )?;
                    }
                }
                "project_task" => {
                    if let Some(project_id) = &task.project_id {
                        project_tasks
                            .entry(project_id.clone())
                            .or_default()
                            .push(task);
                    }
                }
                _ => {}
            }
        }

        for tasks in project_tasks.values_mut() {
            tasks.sort_by(|left, right| {
                left.ordinal
                    .unwrap_or(i64::MAX)
                    .cmp(&right.ordinal.unwrap_or(i64::MAX))
                    .then_with(|| left.created_at.cmp(&right.created_at))
                    .then_with(|| left.id.cmp(&right.id))
            });

            let first_incomplete = tasks.iter().position(|task| task_accepts_execution(task));

            for (index, task) in tasks.iter().enumerate() {
                if !task_accepts_execution(task) {
                    continue;
                }
                let desired_status = if Some(index) == first_incomplete {
                    "ready"
                } else {
                    "waiting_dependency"
                };
                reconcile_work_item_execution(
                    &tx,
                    &mut result,
                    &task.id,
                    "task_implementation",
                    desired_status,
                    repo_remote_url.as_deref(),
                )?;
            }
        }

        tx.commit()?;
        Ok(result)
    }

    pub fn start_execution_run(
        &self,
        execution_id: &str,
        agent_id: &str,
        cube_repo_id: &str,
        cube_lease_id: &str,
        cube_workspace_id: &str,
        workspace_path: &str,
    ) -> Result<(WorkExecution, WorkRun)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
        if execution.status != "ready" {
            bail!(
                "execution {execution_id} is not ready and cannot start a run from status `{}`",
                execution.status
            );
        }

        let now = now_string();
        tx.execute(
            "UPDATE work_executions
             SET status = 'running',
                 cube_repo_id = ?2,
                 cube_lease_id = ?3,
                 cube_workspace_id = ?4,
                 workspace_path = ?5,
                 started_at = COALESCE(started_at, ?6),
                 finished_at = NULL
             WHERE id = ?1",
            params![
                execution_id,
                cube_repo_id,
                cube_lease_id,
                cube_workspace_id,
                workspace_path,
                now
            ],
        )?;

        // Auto-advance the work item's kanban status to `active` so
        // the card moves into the Doing column when work begins.
        // Only applies to tasks/chores; products and projects use a
        // different status vocabulary and aren't rendered on the
        // kanban. Don't downgrade items already in `done` or
        // `archived` — manual transitions win.
        tx.execute(
            "UPDATE tasks
             SET status = 'active',
                 updated_at = ?2
             WHERE id = ?1
               AND deleted_at IS NULL
               AND status NOT IN ('done', 'archived')",
            params![execution.work_item_id, now],
        )?;

        let run_id = next_id("run");
        tx.execute(
            "INSERT INTO work_runs (
                id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                artifacts_path, created_at, started_at, finished_at
             ) VALUES (?1, ?2, ?3, 'active', NULL, NULL, NULL, NULL, ?4, ?4, NULL)",
            params![run_id, execution_id, agent_id, now],
        )?;

        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
        let run = query_run(&tx, &run_id)?
            .with_context(|| format!("missing run after insert: {run_id}"))?;
        tx.commit()?;
        Ok((execution, run))
    }

    pub fn fail_execution_start(
        &self,
        execution_id: &str,
        agent_id: &str,
        cube_repo_id: Option<&str>,
        error_text: &str,
    ) -> Result<(WorkExecution, WorkRun)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
        if execution.status != "ready" {
            bail!(
                "execution {execution_id} is not ready and cannot fail startup from status `{}`",
                execution.status
            );
        }

        let now = now_string();
        tx.execute(
            "UPDATE work_executions
             SET status = 'failed',
                 cube_repo_id = COALESCE(?2, cube_repo_id),
                 cube_lease_id = NULL,
                 cube_workspace_id = NULL,
                 workspace_path = NULL,
                 started_at = COALESCE(started_at, ?3),
                 finished_at = ?3
             WHERE id = ?1",
            params![execution_id, cube_repo_id, now],
        )?;

        let run_id = next_id("run");
        tx.execute(
            "INSERT INTO work_runs (
                id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                artifacts_path, created_at, started_at, finished_at
             ) VALUES (?1, ?2, ?3, 'failed', ?4, NULL, NULL, NULL, ?5, ?5, ?5)",
            params![run_id, execution_id, agent_id, error_text, now],
        )?;

        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
        let run = query_run(&tx, &run_id)?
            .with_context(|| format!("missing run after insert: {run_id}"))?;
        tx.commit()?;
        Ok((execution, run))
    }

    pub fn finish_execution_run(
        &self,
        execution_id: &str,
        run_id: &str,
        execution_status: &str,
        run_status: &str,
        result_summary: Option<&str>,
        error_text: Option<&str>,
        clear_workspace_lease: bool,
        attention: Option<CreateAttentionItemInput>,
    ) -> Result<(WorkExecution, WorkRun, Option<WorkAttentionItem>)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
        if execution.status != "running" {
            bail!(
                "execution {execution_id} is not running and cannot finish a run from status `{}`",
                execution.status
            );
        }

        let run = query_run(&tx, run_id)?.with_context(|| format!("unknown run: {run_id}"))?;
        if run.execution_id != execution_id {
            bail!("run {run_id} does not belong to execution {execution_id}");
        }
        if run.status != "active" {
            bail!(
                "run {run_id} is not active and cannot be finished from status `{}`",
                run.status
            );
        }

        let now = now_string();
        let execution_finished_at = if execution_status_is_terminal(execution_status) {
            Some(now.as_str())
        } else {
            None
        };
        let normalized_result_summary = normalize_optional_text(result_summary.map(str::to_owned));
        let normalized_error_text = normalize_optional_text(error_text.map(str::to_owned));

        tx.execute(
            "UPDATE work_executions
             SET status = ?2,
                 cube_lease_id = CASE WHEN ?3 THEN NULL ELSE cube_lease_id END,
                 cube_workspace_id = CASE WHEN ?3 THEN NULL ELSE cube_workspace_id END,
                 workspace_path = CASE WHEN ?3 THEN NULL ELSE workspace_path END,
                 finished_at = ?4
             WHERE id = ?1",
            params![
                execution_id,
                execution_status,
                clear_workspace_lease,
                execution_finished_at,
            ],
        )?;

        tx.execute(
            "UPDATE work_runs
             SET status = ?2,
                 error_text = ?3,
                 result_summary = ?4,
                 finished_at = ?5
             WHERE id = ?1",
            params![
                run_id,
                run_status,
                normalized_error_text,
                normalized_result_summary,
                now,
            ],
        )?;

        let attention_item = if let Some(input) = attention {
            if input.execution_id != execution_id {
                bail!(
                    "attention item execution `{}` does not match finished execution `{execution_id}`",
                    input.execution_id
                );
            }

            let attention_id = next_id("attn");
            let status = input.status.unwrap_or_else(|| "open".to_owned());
            let resolved_at = normalize_optional_text(input.resolved_at);
            tx.execute(
                "INSERT INTO work_attention_items (
                    id, execution_id, kind, status, title, body_markdown, created_at, resolved_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    attention_id,
                    execution_id,
                    input.kind,
                    status,
                    input.title,
                    input.body_markdown,
                    now,
                    resolved_at,
                ],
            )?;

            Some(
                query_attention_item(&tx, &attention_id)?.with_context(|| {
                    format!("missing attention item after insert: {attention_id}")
                })?,
            )
        } else {
            None
        };

        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
        let run = query_run(&tx, run_id)?.with_context(|| format!("unknown run: {run_id}"))?;
        tx.commit()?;
        Ok((execution, run, attention_item))
    }

    pub fn create_run(&self, input: CreateRunInput) -> Result<WorkRun> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_execution_exists(&tx, &input.execution_id)?;

        let id = next_id("run");
        let now = now_string();
        let status = input.status.unwrap_or_else(|| "starting".to_owned());
        let error_text = normalize_optional_text(input.error_text);
        let result_summary = normalize_optional_text(input.result_summary);
        let transcript_path = normalize_optional_text(input.transcript_path);
        let artifacts_path = normalize_optional_text(input.artifacts_path);
        let started_at = normalize_optional_text(input.started_at);
        let finished_at = normalize_optional_text(input.finished_at);

        tx.execute(
            "INSERT INTO work_runs (
                id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                artifacts_path, created_at, started_at, finished_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                id,
                input.execution_id,
                input.agent_id,
                status,
                error_text,
                result_summary,
                transcript_path,
                artifacts_path,
                now,
                started_at,
                finished_at,
            ],
        )?;

        let run =
            query_run(&tx, &id)?.with_context(|| format!("missing run after insert: {id}"))?;
        tx.commit()?;
        Ok(run)
    }

    pub fn list_runs(&self, execution_id: &str) -> Result<Vec<WorkRun>> {
        let conn = self.connect()?;
        ensure_execution_exists(&conn, execution_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                    artifacts_path, created_at, started_at, finished_at
             FROM work_runs
             WHERE execution_id = ?1
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([execution_id], map_run)?;
        collect_rows(rows)
    }

    pub fn get_run(&self, id: &str) -> Result<WorkRun> {
        let conn = self.connect()?;
        query_run(&conn, id)?.with_context(|| format!("unknown run: {id}"))
    }

    pub fn create_attention_item(
        &self,
        input: CreateAttentionItemInput,
    ) -> Result<WorkAttentionItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_execution_exists(&tx, &input.execution_id)?;

        let id = next_id("attn");
        let now = now_string();
        let status = input.status.unwrap_or_else(|| "open".to_owned());
        let resolved_at = normalize_optional_text(input.resolved_at);

        tx.execute(
            "INSERT INTO work_attention_items (
                id, execution_id, kind, status, title, body_markdown, created_at, resolved_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                input.execution_id,
                input.kind,
                status,
                input.title,
                input.body_markdown,
                now,
                resolved_at,
            ],
        )?;

        let item = query_attention_item(&tx, &id)?
            .with_context(|| format!("missing attention item after insert: {id}"))?;
        tx.commit()?;
        Ok(item)
    }

    pub fn list_attention_items(&self, execution_id: &str) -> Result<Vec<WorkAttentionItem>> {
        let conn = self.connect()?;
        ensure_execution_exists(&conn, execution_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, execution_id, kind, status, title, body_markdown, created_at, resolved_at
             FROM work_attention_items
             WHERE execution_id = ?1
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([execution_id], map_attention_item)?;
        collect_rows(rows)
    }

    pub fn get_attention_item(&self, id: &str) -> Result<WorkAttentionItem> {
        let conn = self.connect()?;
        query_attention_item(&conn, id)?.with_context(|| format!("unknown attention item: {id}"))
    }

    pub fn update_work_item(&self, id: &str, patch: WorkItemPatch) -> Result<WorkItem> {
        match classify_id(id)? {
            ItemKind::Product => self.update_product(id, patch),
            ItemKind::Project => self.update_project(id, patch),
            ItemKind::Task => self.update_task(id, patch),
        }
    }

    pub fn delete_work_item(&self, id: &str) -> Result<()> {
        match classify_id(id)? {
            ItemKind::Task => {
                let mut conn = self.connect()?;
                let tx = conn.transaction()?;
                let now = now_string();
                let rows = tx.execute(
                    "UPDATE tasks SET deleted_at = ?2, updated_at = ?2
                     WHERE id = ?1 AND deleted_at IS NULL",
                    params![id, now],
                )?;
                if rows == 0 {
                    bail!("unknown task: {id}");
                }
                tx.commit()?;
                Ok(())
            }
            ItemKind::Product => bail!("product deletion is not supported; archive it instead"),
            ItemKind::Project => bail!("project deletion is not supported; archive it instead"),
        }
    }

    pub fn get_work_tree(&self, product_id: &str) -> Result<WorkTree> {
        let conn = self.connect()?;
        let product = query_product(&conn, product_id)?
            .with_context(|| format!("unknown product: {product_id}"))?;

        let projects = {
            let mut stmt = conn.prepare(
                "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at
                 FROM projects
                 WHERE product_id = ?1
                 ORDER BY created_at ASC, name COLLATE NOCASE ASC",
            )?;
            let rows = stmt.query_map([product_id], map_project)?;
            collect_rows(rows)?
        };

        let tasks = {
            let mut stmt = conn.prepare(
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at
                 FROM tasks
                 WHERE product_id = ?1 AND kind = 'project_task' AND deleted_at IS NULL
                 ORDER BY COALESCE(ordinal, 0) ASC, created_at ASC",
            )?;
            let rows = stmt.query_map([product_id], map_task)?;
            collect_rows(rows)?
        };

        let chores = {
            let mut stmt = conn.prepare(
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at
                 FROM tasks
                 WHERE product_id = ?1 AND kind = 'chore' AND deleted_at IS NULL
                 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map([product_id], map_task)?;
            collect_rows(rows)?
        };

        Ok(WorkTree {
            product,
            projects,
            tasks,
            chores,
        })
    }

    pub fn reorder_project_tasks(&self, project_id: &str, task_ids: &[String]) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_project_exists(&tx, project_id)?;

        let mut existing = {
            let mut stmt = tx.prepare(
                "SELECT id
                 FROM tasks
                 WHERE project_id = ?1 AND kind = 'project_task' AND deleted_at IS NULL
                 ORDER BY COALESCE(ordinal, 0) ASC, created_at ASC",
            )?;
            let rows = stmt.query_map([project_id], |row| row.get::<_, String>(0))?;
            collect_rows(rows)?
        };
        let mut requested = task_ids.to_vec();
        existing.sort();
        requested.sort();
        if existing != requested {
            bail!("reorder request must include the full active task set for the project");
        }

        for (index, task_id) in task_ids.iter().enumerate() {
            tx.execute(
                "UPDATE tasks SET ordinal = ?2, updated_at = ?3 WHERE id = ?1",
                params![task_id, (index as i64) + 1, now_string()],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    pub fn get_work_item(&self, id: &str) -> Result<WorkItem> {
        let conn = self.connect()?;
        match classify_id(id)? {
            ItemKind::Product => query_product(&conn, id)?
                .map(WorkItem::Product)
                .with_context(|| format!("unknown product: {id}")),
            ItemKind::Project => query_project(&conn, id)?
                .map(WorkItem::Project)
                .with_context(|| format!("unknown project: {id}")),
            ItemKind::Task => query_task(&conn, id)?
                .filter(|task| task.deleted_at.is_none())
                .map(task_to_item)
                .with_context(|| format!("unknown task: {id}")),
        }
    }

    pub fn list_tasks(&self, product_id: &str, project_id: Option<&str>) -> Result<Vec<Task>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;

        if let Some(project_id) = project_id {
            ensure_project_belongs_to_product(&conn, project_id, product_id)?;
            let mut stmt = conn.prepare(
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at
                 FROM tasks
                 WHERE product_id = ?1 AND project_id = ?2 AND kind = 'project_task' AND deleted_at IS NULL
                 ORDER BY COALESCE(ordinal, 0) ASC, created_at ASC",
            )?;
            let rows = stmt.query_map(params![product_id, project_id], map_task)?;
            return collect_rows(rows);
        }

        let mut stmt = conn.prepare(
            "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at
             FROM tasks
             WHERE product_id = ?1 AND kind = 'project_task' AND deleted_at IS NULL
             ORDER BY COALESCE(ordinal, 0) ASC, created_at ASC",
        )?;
        let rows = stmt.query_map([product_id], map_task)?;
        collect_rows(rows)
    }

    /// Look up a cached pane-titlebar summary for a work item.
    /// Returns `(summary, basis_hash)` so callers can compare the
    /// stored basis against a freshly computed one to decide whether
    /// the cache is still valid.
    pub fn get_pane_summary(&self, work_item_id: &str) -> Result<Option<(String, String)>> {
        let conn = self.connect()?;
        let row = conn
            .query_row(
                "SELECT summary, basis_hash FROM pane_summaries WHERE work_item_id = ?1",
                params![work_item_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// Insert or replace the cached pane summary for a work item.
    /// `basis_hash` should be derived from the inputs that, if
    /// changed, invalidate the cached summary (typically a hash of
    /// name + description).
    pub fn set_pane_summary(
        &self,
        work_item_id: &str,
        summary: &str,
        basis_hash: &str,
    ) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO pane_summaries (work_item_id, summary, basis_hash, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(work_item_id) DO UPDATE SET
                 summary = excluded.summary,
                 basis_hash = excluded.basis_hash,
                 created_at = excluded.created_at",
            params![work_item_id, summary, basis_hash, now_string()],
        )?;
        Ok(())
    }

    pub fn list_chores(&self, product_id: &str) -> Result<Vec<Task>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at
             FROM tasks
             WHERE product_id = ?1 AND kind = 'chore' AND deleted_at IS NULL
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([product_id], map_task)?;
        collect_rows(rows)
    }

    fn init(&self) -> Result<()> {
        let conn = self.connect()?;
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS metadata (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS products (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                slug TEXT NOT NULL UNIQUE,
                description TEXT NOT NULL DEFAULT '',
                repo_remote_url TEXT,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY,
                product_id TEXT NOT NULL REFERENCES products(id),
                name TEXT NOT NULL,
                slug TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                goal TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL,
                priority TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE UNIQUE INDEX IF NOT EXISTS projects_product_slug_idx
                ON projects(product_id, slug);

            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                product_id TEXT NOT NULL REFERENCES products(id),
                project_id TEXT REFERENCES projects(id),
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL,
                ordinal INTEGER,
                pr_url TEXT,
                deleted_at TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS tasks_product_idx
                ON tasks(product_id, kind, deleted_at);

            CREATE INDEX IF NOT EXISTS tasks_project_idx
                ON tasks(project_id, deleted_at, ordinal);

            CREATE TABLE IF NOT EXISTS work_executions (
                id TEXT PRIMARY KEY,
                work_item_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                repo_remote_url TEXT NOT NULL,
                cube_repo_id TEXT,
                cube_lease_id TEXT,
                cube_workspace_id TEXT,
                workspace_path TEXT,
                priority INTEGER NOT NULL DEFAULT 0,
                preferred_workspace_id TEXT,
                created_at TEXT NOT NULL,
                started_at TEXT,
                finished_at TEXT
            );

            CREATE INDEX IF NOT EXISTS work_executions_work_item_idx
                ON work_executions(work_item_id, created_at);

            CREATE TABLE IF NOT EXISTS work_runs (
                id TEXT PRIMARY KEY,
                execution_id TEXT NOT NULL REFERENCES work_executions(id) ON DELETE CASCADE,
                agent_id TEXT NOT NULL,
                status TEXT NOT NULL,
                error_text TEXT,
                result_summary TEXT,
                transcript_path TEXT,
                artifacts_path TEXT,
                created_at TEXT NOT NULL,
                started_at TEXT,
                finished_at TEXT
            );

            CREATE INDEX IF NOT EXISTS work_runs_execution_idx
                ON work_runs(execution_id, created_at);

            CREATE TABLE IF NOT EXISTS work_attention_items (
                id TEXT PRIMARY KEY,
                execution_id TEXT NOT NULL REFERENCES work_executions(id) ON DELETE CASCADE,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                title TEXT NOT NULL,
                body_markdown TEXT NOT NULL,
                created_at TEXT NOT NULL,
                resolved_at TEXT
            );

            CREATE INDEX IF NOT EXISTS work_attention_items_execution_idx
                ON work_attention_items(execution_id, created_at);

            CREATE TABLE IF NOT EXISTS pane_summaries (
                work_item_id TEXT PRIMARY KEY,
                summary TEXT NOT NULL,
                basis_hash TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            ",
        )?;
        migrate_work_executions_v3(&conn)?;
        // Index creation must follow migration: pre-v3 databases don't
        // have `priority` until `migrate_work_executions_v3` adds it,
        // and SQLite's `CREATE INDEX IF NOT EXISTS` errors on missing
        // columns rather than silently skipping. Keep this out of the
        // schema-init batch so a pre-v3 database can still be opened.
        conn.execute(
            "CREATE INDEX IF NOT EXISTS work_executions_ready_idx
                ON work_executions(status, priority, created_at)",
            [],
        )?;
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES ('schema_version', '3')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )?;
        Ok(())
    }

    fn connect(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path)
            .with_context(|| format!("failed to open work db {}", self.path.display()))?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(conn)
    }

    fn update_product(&self, id: &str, patch: WorkItemPatch) -> Result<WorkItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut product =
            query_product(&tx, id)?.with_context(|| format!("unknown product: {id}"))?;

        apply_text_patch(&mut product.name, patch.name);
        apply_text_patch(&mut product.description, patch.description);
        apply_optional_patch(&mut product.repo_remote_url, patch.repo_remote_url);
        apply_text_patch(&mut product.status, patch.status);
        product.slug = unique_product_slug_for_update(&tx, id, &slugify(&product.name))?;
        product.updated_at = now_string();

        tx.execute(
            "UPDATE products
             SET name = ?2, slug = ?3, description = ?4, repo_remote_url = ?5, status = ?6, updated_at = ?7
             WHERE id = ?1",
            params![
                product.id,
                product.name,
                product.slug,
                product.description,
                product.repo_remote_url,
                product.status,
                product.updated_at,
            ],
        )?;

        let updated = query_product(&tx, id)?.with_context(|| format!("unknown product: {id}"))?;
        tx.commit()?;
        Ok(WorkItem::Product(updated))
    }

    fn update_project(&self, id: &str, patch: WorkItemPatch) -> Result<WorkItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut project =
            query_project(&tx, id)?.with_context(|| format!("unknown project: {id}"))?;

        apply_text_patch(&mut project.name, patch.name);
        apply_text_patch(&mut project.description, patch.description);
        apply_text_patch(&mut project.goal, patch.goal);
        apply_text_patch(&mut project.status, patch.status);
        apply_text_patch(&mut project.priority, patch.priority);
        project.slug =
            unique_project_slug_for_update(&tx, &project.product_id, id, &slugify(&project.name))?;
        project.updated_at = now_string();

        tx.execute(
            "UPDATE projects
             SET name = ?2, slug = ?3, description = ?4, goal = ?5, status = ?6, priority = ?7, updated_at = ?8
             WHERE id = ?1",
            params![
                project.id,
                project.name,
                project.slug,
                project.description,
                project.goal,
                project.status,
                project.priority,
                project.updated_at,
            ],
        )?;

        let updated = query_project(&tx, id)?.with_context(|| format!("unknown project: {id}"))?;
        tx.commit()?;
        Ok(WorkItem::Project(updated))
    }

    fn update_task(&self, id: &str, patch: WorkItemPatch) -> Result<WorkItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut task = query_task(&tx, id)?.with_context(|| format!("unknown task: {id}"))?;
        if task.deleted_at.is_some() {
            bail!("cannot update a deleted task: {id}");
        }

        apply_text_patch(&mut task.name, patch.name);
        apply_text_patch(&mut task.description, patch.description);
        apply_text_patch(&mut task.status, patch.status);
        apply_optional_patch(&mut task.pr_url, patch.pr_url);
        if let Some(ordinal) = patch.ordinal {
            task.ordinal = Some(ordinal);
        }
        task.updated_at = now_string();

        tx.execute(
            "UPDATE tasks
             SET name = ?2, description = ?3, status = ?4, ordinal = ?5, pr_url = ?6, updated_at = ?7
             WHERE id = ?1",
            params![
                task.id,
                task.name,
                task.description,
                task.status,
                task.ordinal,
                task.pr_url,
                task.updated_at,
            ],
        )?;

        let updated = query_task(&tx, id)?.with_context(|| format!("unknown task: {id}"))?;
        tx.commit()?;
        Ok(task_to_item(updated))
    }
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&Row<'_>) -> rusqlite::Result<T>>,
) -> Result<Vec<T>> {
    let mut values = Vec::new();
    for row in rows {
        values.push(row?);
    }
    Ok(values)
}

fn map_product(row: &Row<'_>) -> rusqlite::Result<Product> {
    Ok(Product {
        id: row.get(0)?,
        name: row.get(1)?,
        slug: row.get(2)?,
        description: row.get(3)?,
        repo_remote_url: row.get(4)?,
        status: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn map_project(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        product_id: row.get(1)?,
        name: row.get(2)?,
        slug: row.get(3)?,
        description: row.get(4)?,
        goal: row.get(5)?,
        status: row.get(6)?,
        priority: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn map_task(row: &Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get(0)?,
        product_id: row.get(1)?,
        project_id: row.get(2)?,
        kind: row.get(3)?,
        name: row.get(4)?,
        description: row.get(5)?,
        status: row.get(6)?,
        ordinal: row.get(7)?,
        pr_url: row.get(8)?,
        deleted_at: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

fn map_execution(row: &Row<'_>) -> rusqlite::Result<WorkExecution> {
    Ok(WorkExecution {
        id: row.get(0)?,
        work_item_id: row.get(1)?,
        kind: row.get(2)?,
        status: row.get(3)?,
        repo_remote_url: row.get(4)?,
        cube_repo_id: row.get(5)?,
        cube_lease_id: row.get(6)?,
        cube_workspace_id: row.get(7)?,
        workspace_path: row.get(8)?,
        priority: row.get(9)?,
        preferred_workspace_id: row.get(10)?,
        created_at: row.get(11)?,
        started_at: row.get(12)?,
        finished_at: row.get(13)?,
    })
}

fn map_run(row: &Row<'_>) -> rusqlite::Result<WorkRun> {
    Ok(WorkRun {
        id: row.get(0)?,
        execution_id: row.get(1)?,
        agent_id: row.get(2)?,
        status: row.get(3)?,
        error_text: row.get(4)?,
        result_summary: row.get(5)?,
        transcript_path: row.get(6)?,
        artifacts_path: row.get(7)?,
        created_at: row.get(8)?,
        started_at: row.get(9)?,
        finished_at: row.get(10)?,
    })
}

fn map_attention_item(row: &Row<'_>) -> rusqlite::Result<WorkAttentionItem> {
    Ok(WorkAttentionItem {
        id: row.get(0)?,
        execution_id: row.get(1)?,
        kind: row.get(2)?,
        status: row.get(3)?,
        title: row.get(4)?,
        body_markdown: row.get(5)?,
        created_at: row.get(6)?,
        resolved_at: row.get(7)?,
    })
}

fn insert_execution(conn: &Connection, input: CreateExecutionInput) -> Result<WorkExecution> {
    let repo_remote_url = resolve_execution_repo_remote_url(
        conn,
        &input.work_item_id,
        normalize_optional_text(input.repo_remote_url),
    )?;
    let id = next_id("exec");
    let now = now_string();
    let status = input.status.unwrap_or_else(|| "queued".to_owned());
    let cube_repo_id = normalize_optional_text(input.cube_repo_id);
    let cube_lease_id = normalize_optional_text(input.cube_lease_id);
    let cube_workspace_id = normalize_optional_text(input.cube_workspace_id);
    let workspace_path = normalize_optional_text(input.workspace_path);
    let priority = input.priority.unwrap_or(0);
    let preferred_workspace_id = normalize_optional_text(input.preferred_workspace_id);
    let started_at = normalize_optional_text(input.started_at);
    let finished_at = normalize_optional_text(input.finished_at);

    conn.execute(
        "INSERT INTO work_executions (
            id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
            cube_workspace_id, workspace_path, priority, preferred_workspace_id,
            created_at, started_at, finished_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            id,
            input.work_item_id,
            input.kind,
            status,
            repo_remote_url,
            cube_repo_id,
            cube_lease_id,
            cube_workspace_id,
            workspace_path,
            priority,
            preferred_workspace_id,
            now,
            started_at,
            finished_at,
        ],
    )?;

    query_execution(conn, &id)?.with_context(|| format!("missing execution after insert: {id}"))
}

fn query_product(conn: &Connection, id: &str) -> Result<Option<Product>> {
    conn.query_row(
        "SELECT id, name, slug, description, repo_remote_url, status, created_at, updated_at
         FROM products
         WHERE id = ?1",
        [id],
        map_product,
    )
    .optional()
    .map_err(Into::into)
}

fn query_project(conn: &Connection, id: &str) -> Result<Option<Project>> {
    conn.query_row(
        "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at
         FROM projects
         WHERE id = ?1",
        [id],
        map_project,
    )
    .optional()
    .map_err(Into::into)
}

fn query_task(conn: &Connection, id: &str) -> Result<Option<Task>> {
    conn.query_row(
        "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at
         FROM tasks
         WHERE id = ?1",
        [id],
        map_task,
    )
    .optional()
    .map_err(Into::into)
}

fn query_execution(conn: &Connection, id: &str) -> Result<Option<WorkExecution>> {
    conn.query_row(
        "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                created_at, started_at, finished_at
         FROM work_executions
         WHERE id = ?1",
        [id],
        map_execution,
    )
    .optional()
    .map_err(Into::into)
}

fn query_run(conn: &Connection, id: &str) -> Result<Option<WorkRun>> {
    conn.query_row(
        "SELECT id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                artifacts_path, created_at, started_at, finished_at
         FROM work_runs
         WHERE id = ?1",
        [id],
        map_run,
    )
    .optional()
    .map_err(Into::into)
}

fn query_attention_item(conn: &Connection, id: &str) -> Result<Option<WorkAttentionItem>> {
    conn.query_row(
        "SELECT id, execution_id, kind, status, title, body_markdown, created_at, resolved_at
         FROM work_attention_items
         WHERE id = ?1",
        [id],
        map_attention_item,
    )
    .optional()
    .map_err(Into::into)
}

fn list_projects_for_product(conn: &Connection, product_id: &str) -> Result<Vec<Project>> {
    let mut stmt = conn.prepare(
        "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at
         FROM projects
         WHERE product_id = ?1
         ORDER BY created_at ASC, name COLLATE NOCASE ASC",
    )?;
    let rows = stmt.query_map([product_id], map_project)?;
    collect_rows(rows)
}

fn list_tasks_for_product(conn: &Connection, product_id: &str) -> Result<Vec<Task>> {
    let mut stmt = conn.prepare(
        "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at
         FROM tasks
         WHERE product_id = ?1 AND deleted_at IS NULL
         ORDER BY project_id ASC, ordinal ASC, created_at ASC, id ASC",
    )?;
    let rows = stmt.query_map([product_id], map_task)?;
    collect_rows(rows)
}

fn ensure_product_exists(conn: &Connection, product_id: &str) -> Result<()> {
    let exists = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM products WHERE id = ?1)",
        [product_id],
        |row| row.get::<_, i64>(0),
    )?;
    if exists == 0 {
        bail!("unknown product: {product_id}");
    }
    Ok(())
}

fn ensure_project_exists(conn: &Connection, project_id: &str) -> Result<()> {
    let exists = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1)",
        [project_id],
        |row| row.get::<_, i64>(0),
    )?;
    if exists == 0 {
        bail!("unknown project: {project_id}");
    }
    Ok(())
}

fn ensure_project_belongs_to_product(
    conn: &Connection,
    project_id: &str,
    product_id: &str,
) -> Result<()> {
    let exists = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1 AND product_id = ?2)",
        params![project_id, product_id],
        |row| row.get::<_, i64>(0),
    )?;
    if exists == 0 {
        bail!("project {project_id} does not belong to product {product_id}");
    }
    Ok(())
}

fn migrate_work_executions_v3(conn: &Connection) -> Result<()> {
    for (column, ddl) in [
        (
            "cube_workspace_id",
            "ALTER TABLE work_executions ADD COLUMN cube_workspace_id TEXT",
        ),
        (
            "priority",
            "ALTER TABLE work_executions ADD COLUMN priority INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "preferred_workspace_id",
            "ALTER TABLE work_executions ADD COLUMN preferred_workspace_id TEXT",
        ),
    ] {
        if !work_executions_has_column(conn, column)? {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
}

fn work_executions_has_column(conn: &Connection, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare("PRAGMA table_info(work_executions)")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn ensure_execution_exists(conn: &Connection, execution_id: &str) -> Result<()> {
    let exists = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_executions WHERE id = ?1)",
        [execution_id],
        |row| row.get::<_, i64>(0),
    )?;
    if exists == 0 {
        bail!("unknown execution: {execution_id}");
    }
    Ok(())
}

fn query_latest_execution_for_work_item(
    conn: &Connection,
    work_item_id: &str,
) -> Result<Option<WorkExecution>> {
    conn.query_row(
        "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                created_at, started_at, finished_at
         FROM work_executions
         WHERE work_item_id = ?1
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
        [work_item_id],
        map_execution,
    )
    .optional()
    .map_err(Into::into)
}

fn reconcile_work_item_execution(
    conn: &Connection,
    result: &mut ExecutionReconcileResult,
    work_item_id: &str,
    kind: &str,
    desired_status: &str,
    repo_remote_url: Option<&str>,
) -> Result<()> {
    match query_latest_execution_for_work_item(conn, work_item_id)? {
        Some(execution) => {
            if execution.kind == kind
                && can_reconcile_execution_status(&execution.status)
                && execution.status != desired_status
            {
                let updated = update_execution_status(conn, &execution.id, desired_status)?;
                result.updated.push(updated);
            }
        }
        None => {
            let Some(repo_remote_url) = repo_remote_url else {
                return Ok(());
            };
            let created = insert_execution(
                conn,
                CreateExecutionInput {
                    work_item_id: work_item_id.to_owned(),
                    kind: kind.to_owned(),
                    status: Some(desired_status.to_owned()),
                    repo_remote_url: Some(repo_remote_url.to_owned()),
                    cube_repo_id: None,
                    cube_lease_id: None,
                    cube_workspace_id: None,
                    workspace_path: None,
                    priority: None,
                    preferred_workspace_id: None,
                    started_at: None,
                    finished_at: None,
                },
            )?;
            result.created.push(created);
        }
    }

    Ok(())
}

fn request_execution_in_tx(
    conn: &Connection,
    input: RequestExecutionInput,
) -> Result<WorkExecution> {
    let RequestExecutionInput {
        work_item_id,
        priority,
        preferred_workspace_id,
    } = input;

    let preferred_workspace_id = normalize_optional_text(preferred_workspace_id);
    let kind = execution_kind_for_work_item(conn, &work_item_id)?;

    if let Some(existing) = query_latest_execution_for_work_item(conn, &work_item_id)? {
        if !execution_status_is_terminal(&existing.status) {
            let next_status = if existing.status == "waiting_dependency" {
                "ready".to_owned()
            } else {
                existing.status.clone()
            };
            let next_priority = priority.unwrap_or(existing.priority);
            let next_preferred = preferred_workspace_id.or(existing.preferred_workspace_id);
            conn.execute(
                "UPDATE work_executions
                 SET status = ?2,
                     priority = ?3,
                     preferred_workspace_id = ?4
                 WHERE id = ?1",
                params![existing.id, next_status, next_priority, next_preferred],
            )?;
            return query_execution(conn, &existing.id)?
                .with_context(|| format!("unknown execution: {}", existing.id));
        }
    }

    let _ = product_id_for_work_item(conn, &work_item_id)?;
    insert_execution(
        conn,
        CreateExecutionInput {
            work_item_id,
            kind,
            status: Some("ready".to_owned()),
            repo_remote_url: None,
            cube_repo_id: None,
            cube_lease_id: None,
            cube_workspace_id: None,
            workspace_path: None,
            priority,
            preferred_workspace_id,
            started_at: None,
            finished_at: None,
        },
    )
}

fn execution_kind_for_work_item(conn: &Connection, work_item_id: &str) -> Result<String> {
    Ok(match classify_id(work_item_id)? {
        ItemKind::Product => "product_design".to_owned(),
        ItemKind::Project => "project_design".to_owned(),
        ItemKind::Task => {
            let task = query_task(conn, work_item_id)?
                .filter(|task| task.deleted_at.is_none())
                .with_context(|| format!("unknown task: {work_item_id}"))?;
            match task.kind.as_str() {
                "chore" => "chore_implementation".to_owned(),
                _ => "task_implementation".to_owned(),
            }
        }
    })
}

fn update_execution_status(
    conn: &Connection,
    execution_id: &str,
    status: &str,
) -> Result<WorkExecution> {
    let updated = conn.execute(
        "UPDATE work_executions SET status = ?2 WHERE id = ?1",
        params![execution_id, status],
    )?;
    if updated == 0 {
        bail!("unknown execution: {execution_id}");
    }

    query_execution(conn, execution_id)?
        .with_context(|| format!("unknown execution: {execution_id}"))
}

fn can_reconcile_execution_status(status: &str) -> bool {
    matches!(status, "queued" | "ready" | "waiting_dependency")
}

fn execution_status_is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "abandoned")
}

fn project_accepts_execution(project: &Project) -> bool {
    !matches!(project.status.as_str(), "done" | "archived")
}

fn task_accepts_execution(task: &Task) -> bool {
    task.deleted_at.is_none() && task.status != "done"
}

fn product_id_for_work_item(conn: &Connection, work_item_id: &str) -> Result<String> {
    match classify_id(work_item_id)? {
        ItemKind::Product => query_product(conn, work_item_id)?
            .map(|product| product.id)
            .with_context(|| format!("unknown product: {work_item_id}")),
        ItemKind::Project => query_project(conn, work_item_id)?
            .map(|project| project.product_id)
            .with_context(|| format!("unknown project: {work_item_id}")),
        ItemKind::Task => query_task(conn, work_item_id)?
            .filter(|task| task.deleted_at.is_none())
            .map(|task| task.product_id)
            .with_context(|| format!("unknown task: {work_item_id}")),
    }
}

fn resolve_execution_repo_remote_url(
    conn: &Connection,
    work_item_id: &str,
    explicit_repo_remote_url: Option<String>,
) -> Result<String> {
    if let Some(repo_remote_url) = explicit_repo_remote_url {
        let _ = product_id_for_work_item(conn, work_item_id)?;
        return Ok(repo_remote_url);
    }

    let product_id = product_id_for_work_item(conn, work_item_id)?;
    let product = query_product(conn, &product_id)?
        .with_context(|| format!("unknown product: {product_id}"))?;
    product.repo_remote_url.with_context(|| {
        format!(
            "work item {work_item_id} does not resolve to a product repo_remote_url; provide one explicitly"
        )
    })
}

fn next_task_ordinal(conn: &Connection, project_id: &str) -> Result<i64> {
    let current = conn.query_row(
        "SELECT COALESCE(MAX(ordinal), 0) FROM tasks
             WHERE project_id = ?1 AND kind = 'project_task' AND deleted_at IS NULL",
        [project_id],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(current + 1)
}

fn unique_product_slug(conn: &Connection, base_slug: &str) -> Result<String> {
    let base_slug = default_slug(base_slug);
    let mut candidate = base_slug.clone();
    let mut suffix = 2;
    while conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM products WHERE slug = ?1)",
        [candidate.as_str()],
        |row| row.get::<_, i64>(0),
    )? != 0
    {
        candidate = format!("{base_slug}-{suffix}");
        suffix += 1;
    }
    Ok(candidate)
}

fn unique_product_slug_for_update(conn: &Connection, id: &str, base_slug: &str) -> Result<String> {
    let base_slug = default_slug(base_slug);
    let mut candidate = base_slug.clone();
    let mut suffix = 2;
    while conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM products WHERE slug = ?1 AND id != ?2)",
        params![candidate, id],
        |row| row.get::<_, i64>(0),
    )? != 0
    {
        candidate = format!("{base_slug}-{suffix}");
        suffix += 1;
    }
    Ok(candidate)
}

fn unique_project_slug(conn: &Connection, product_id: &str, base_slug: &str) -> Result<String> {
    let base_slug = default_slug(base_slug);
    let mut candidate = base_slug.clone();
    let mut suffix = 2;
    while conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE product_id = ?1 AND slug = ?2)",
        params![product_id, candidate],
        |row| row.get::<_, i64>(0),
    )? != 0
    {
        candidate = format!("{base_slug}-{suffix}");
        suffix += 1;
    }
    Ok(candidate)
}

fn unique_project_slug_for_update(
    conn: &Connection,
    product_id: &str,
    id: &str,
    base_slug: &str,
) -> Result<String> {
    let base_slug = default_slug(base_slug);
    let mut candidate = base_slug.clone();
    let mut suffix = 2;
    while conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE product_id = ?1 AND slug = ?2 AND id != ?3)",
        params![product_id, candidate, id],
        |row| row.get::<_, i64>(0),
    )? != 0
    {
        candidate = format!("{base_slug}-{suffix}");
        suffix += 1;
    }
    Ok(candidate)
}

fn default_slug(base_slug: &str) -> String {
    if base_slug.is_empty() {
        "item".to_owned()
    } else {
        base_slug.to_owned()
    }
}

fn next_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{nanos:x}_{counter:x}")
}

fn now_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value.and_then(|text| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

fn apply_text_patch(target: &mut String, patch: Option<String>) {
    if let Some(value) = patch {
        *target = value;
    }
}

fn apply_optional_patch(target: &mut Option<String>, patch: Option<String>) {
    if let Some(value) = patch {
        *target = normalize_optional_text(Some(value));
    }
}

fn task_to_item(task: Task) -> WorkItem {
    if task.kind == "chore" {
        WorkItem::Chore(task)
    } else {
        WorkItem::Task(task)
    }
}

fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;

    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }

    slug.trim_matches('-').to_owned()
}

enum ItemKind {
    Product,
    Project,
    Task,
}

fn classify_id(id: &str) -> Result<ItemKind> {
    if id.starts_with("prod_") {
        return Ok(ItemKind::Product);
    }
    if id.starts_with("proj_") {
        return Ok(ItemKind::Project);
    }
    if id.starts_with("task_") {
        return Ok(ItemKind::Task);
    }
    bail!("unknown work item id format: {id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db_path(label: &str) -> PathBuf {
        let file = format!("boss-{label}-{}.sqlite3", next_id("test"));
        std::env::temp_dir().join(file)
    }

    #[test]
    fn creates_tree_and_soft_deletes_chores() {
        let path = temp_db_path("tree");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: Some("desc".to_owned()),
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Work taxonomy".to_owned(),
                description: None,
                goal: Some("goal".to_owned()),
            })
            .unwrap();
        let task = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "Backend schema".to_owned(),
                description: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
            })
            .unwrap();

        let tree = db.get_work_tree(&product.id).unwrap();
        assert_eq!(tree.projects.len(), 1);
        assert_eq!(tree.tasks.len(), 1);
        assert_eq!(tree.tasks[0].id, task.id);
        assert_eq!(tree.chores.len(), 1);
        assert_eq!(tree.chores[0].id, chore.id);

        db.delete_work_item(&chore.id).unwrap();
        let tree = db.get_work_tree(&product.id).unwrap();
        assert!(tree.chores.is_empty());

        let _ = std::fs::remove_file(path);
    }

    /// A pre-v3 database has `work_executions` without `priority`,
    /// `cube_workspace_id`, or `preferred_workspace_id`. Opening the
    /// db must apply the column migrations and the `priority`-keyed
    /// index without erroring.
    #[test]
    fn opens_pre_v3_database_without_priority_column() {
        let path = temp_db_path("pre-v3");
        // Build a minimal pre-v3 schema: just the table the migration
        // touches, missing the three v3 columns.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE work_executions (
                id TEXT PRIMARY KEY,
                work_item_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                repo_remote_url TEXT NOT NULL,
                cube_repo_id TEXT,
                cube_lease_id TEXT,
                workspace_path TEXT,
                created_at TEXT NOT NULL,
                started_at TEXT,
                finished_at TEXT
            );",
        )
        .unwrap();
        drop(conn);

        // This used to fail with `no such column: priority` because the
        // index DDL was in the same batch as the table DDL, so the
        // migration that adds the column never got a chance to run.
        let db = WorkDb::open(path.clone()).unwrap();

        // Sanity-check that v3 columns are now present and the index
        // exists.
        let conn = db.connect().unwrap();
        let cols: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(work_executions)").unwrap();
            stmt.query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .filter_map(Result::ok)
                .collect()
        };
        assert!(cols.contains(&"priority".to_owned()));
        assert!(cols.contains(&"cube_workspace_id".to_owned()));
        assert!(cols.contains(&"preferred_workspace_id".to_owned()));

        let index_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' AND name = 'work_executions_ready_idx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_exists, 1);

        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reorders_project_tasks() {
        let path = temp_db_path("reorder");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Taxonomy".to_owned(),
                description: None,
                goal: None,
            })
            .unwrap();
        let first = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "One".to_owned(),
                description: None,
            })
            .unwrap();
        let second = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "Two".to_owned(),
                description: None,
            })
            .unwrap();

        db.reorder_project_tasks(&project.id, &[second.id.clone(), first.id.clone()])
            .unwrap();

        let tree = db.get_work_tree(&product.id).unwrap();
        assert_eq!(tree.tasks[0].id, second.id);
        assert_eq!(tree.tasks[1].id, first.id);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn creates_and_lists_execution_entities() {
        let path = temp_db_path("executions");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Execution foundation".to_owned(),
                description: None,
                goal: None,
            })
            .unwrap();
        let task = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "Schema".to_owned(),
                description: None,
            })
            .unwrap();

        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: task.id.clone(),
                kind: "task_implementation".to_owned(),
                status: Some("ready".to_owned()),
                repo_remote_url: None,
                cube_repo_id: Some("cube_repo_mono".to_owned()),
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: Some("/tmp/mono-agent-001".to_owned()),
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();
        assert_eq!(
            execution.repo_remote_url,
            "git@github.com:spinyfin/mono.git"
        );

        let run = db
            .create_run(CreateRunInput {
                execution_id: execution.id.clone(),
                agent_id: "agent_1".to_owned(),
                status: Some("active".to_owned()),
                error_text: None,
                result_summary: Some("started work".to_owned()),
                transcript_path: Some("/tmp/transcript.jsonl".to_owned()),
                artifacts_path: Some("/tmp/artifacts".to_owned()),
                started_at: Some("100".to_owned()),
                finished_at: None,
            })
            .unwrap();
        let attention = db
            .create_attention_item(CreateAttentionItemInput {
                execution_id: execution.id.clone(),
                kind: "decision_required".to_owned(),
                status: Some("open".to_owned()),
                title: "Need product call".to_owned(),
                body_markdown: "Please decide.".to_owned(),
                resolved_at: None,
            })
            .unwrap();

        let executions = db.list_executions(Some(&task.id)).unwrap();
        assert_eq!(executions.len(), 1);
        assert_eq!(executions[0].id, execution.id);
        assert_eq!(db.get_execution(&execution.id).unwrap().id, execution.id);

        let runs = db.list_runs(&execution.id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, run.id);
        assert_eq!(
            db.get_run(&run.id).unwrap().transcript_path.as_deref(),
            Some("/tmp/transcript.jsonl")
        );

        let items = db.list_attention_items(&execution.id).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, attention.id);
        assert_eq!(
            db.get_attention_item(&attention.id).unwrap().title,
            "Need product call"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn execution_requires_repo_remote_url_snapshot() {
        let path = temp_db_path("execution-repo");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();

        let err = db
            .create_execution(CreateExecutionInput {
                work_item_id: product.id.clone(),
                kind: "project_design".to_owned(),
                status: None,
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("does not resolve to a product repo_remote_url")
        );

        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: product.id.clone(),
                kind: "project_design".to_owned(),
                status: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();
        assert_eq!(
            execution.repo_remote_url,
            "git@github.com:spinyfin/mono.git"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reconciles_missing_executions_for_product_tree() {
        let path = temp_db_path("reconcile-tree");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Execution coordinator".to_owned(),
                description: None,
                goal: None,
            })
            .unwrap();
        let first_task = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "First".to_owned(),
                description: None,
            })
            .unwrap();
        let second_task = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "Second".to_owned(),
                description: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
            })
            .unwrap();

        let result = db.reconcile_product_executions(&product.id).unwrap();
        assert_eq!(result.created.len(), 4);
        assert!(result.updated.is_empty());

        let first_execution = db.list_executions(Some(&first_task.id)).unwrap();
        assert_eq!(first_execution.len(), 1);
        assert_eq!(first_execution[0].kind, "task_implementation");
        assert_eq!(first_execution[0].status, "ready");

        let second_execution = db.list_executions(Some(&second_task.id)).unwrap();
        assert_eq!(second_execution.len(), 1);
        assert_eq!(second_execution[0].status, "waiting_dependency");

        let chore_execution = db.list_executions(Some(&chore.id)).unwrap();
        assert_eq!(chore_execution.len(), 1);
        assert_eq!(chore_execution[0].kind, "chore_implementation");
        assert_eq!(chore_execution[0].status, "ready");

        let second_pass = db.reconcile_product_executions(&product.id).unwrap();
        assert!(second_pass.created.is_empty());
        assert!(second_pass.updated.is_empty());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reconcile_promotes_next_project_task_when_previous_done() {
        let path = temp_db_path("reconcile-promote");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Execution coordinator".to_owned(),
                description: None,
                goal: None,
            })
            .unwrap();
        let first_task = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "First".to_owned(),
                description: None,
            })
            .unwrap();
        let second_task = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "Second".to_owned(),
                description: None,
            })
            .unwrap();

        db.reconcile_product_executions(&product.id).unwrap();
        db.update_work_item(
            &first_task.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        let result = db.reconcile_product_executions(&product.id).unwrap();
        assert!(result.created.is_empty());
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].work_item_id, second_task.id);
        assert_eq!(result.updated[0].status, "ready");

        let second_execution = db.list_executions(Some(&second_task.id)).unwrap();
        assert_eq!(second_execution[0].status, "ready");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reconcile_waits_for_product_repo_remote_url() {
        let path = temp_db_path("reconcile-repo");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Execution coordinator".to_owned(),
                description: None,
                goal: None,
            })
            .unwrap();
        let task = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "First".to_owned(),
                description: None,
            })
            .unwrap();

        let first_pass = db.reconcile_product_executions(&product.id).unwrap();
        assert!(first_pass.created.is_empty());
        assert!(db.list_executions(None).unwrap().is_empty());

        db.update_work_item(
            &product.id,
            WorkItemPatch {
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let second_pass = db.reconcile_product_executions(&product.id).unwrap();
        assert_eq!(second_pass.created.len(), 2);

        let task_execution = db.list_executions(Some(&task.id)).unwrap();
        assert_eq!(task_execution.len(), 1);
        assert_eq!(task_execution[0].status, "ready");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn starts_ready_execution_run_and_attaches_workspace() {
        let path = temp_db_path("start-run");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();

        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                "/tmp/mono-agent-001",
            )
            .unwrap();
        assert_eq!(execution.status, "running");
        assert_eq!(execution.cube_repo_id.as_deref(), Some("mono"));
        assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
        assert_eq!(
            execution.workspace_path.as_deref(),
            Some("/tmp/mono-agent-001")
        );
        assert!(execution.started_at.is_some());
        assert_eq!(run.execution_id, execution.id);
        assert_eq!(run.agent_id, "worker-1");
        assert_eq!(run.status, "active");
        assert!(run.started_at.is_some());
        assert!(run.finished_at.is_none());

        // Auto-advance: starting an execution moves the chore's
        // kanban status from `todo` to `active` so it appears in the
        // Doing column.
        let advanced_chore = db.get_work_item(&chore.id).unwrap();
        match advanced_chore {
            WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, "active"),
            other => panic!("expected chore/task, got {other:?}"),
        }

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn start_execution_does_not_downgrade_done_chores() {
        let path = temp_db_path("no-downgrade");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Already done".to_owned(),
                description: None,
            })
            .unwrap();
        // Manually mark the chore as done before starting execution.
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();

        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();

        db.start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();

        // The chore was already `done` — auto-advance must not
        // overwrite that with `active`. Manual transitions win.
        let unchanged = db.get_work_item(&chore.id).unwrap();
        match unchanged {
            WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, "done"),
            other => panic!("expected chore/task, got {other:?}"),
        }

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn records_failed_execution_start_attempt() {
        let path = temp_db_path("fail-run");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();

        let (execution, run) = db
            .fail_execution_start(
                &execution.id,
                "worker-1",
                Some("mono"),
                "cube workspace lease failed",
            )
            .unwrap();
        assert_eq!(execution.status, "failed");
        assert_eq!(execution.cube_repo_id.as_deref(), Some("mono"));
        assert!(execution.cube_lease_id.is_none());
        assert!(execution.workspace_path.is_none());
        assert!(execution.finished_at.is_some());
        assert_eq!(run.status, "failed");
        assert_eq!(
            run.error_text.as_deref(),
            Some("cube workspace lease failed")
        );
        assert!(run.finished_at.is_some());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn finishes_active_run_into_waiting_human_with_attention() {
        let path = temp_db_path("finish-run-waiting");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();

        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                "/tmp/mono-agent-001",
            )
            .unwrap();

        let (execution, run, attention) = db
            .finish_execution_run(
                &execution.id,
                &run.id,
                "waiting_human",
                "completed",
                Some("Implemented the first pass."),
                None,
                false,
                Some(CreateAttentionItemInput {
                    execution_id: execution.id.clone(),
                    kind: "review_required".to_owned(),
                    status: None,
                    title: "Review implementation output for Cleanup".to_owned(),
                    body_markdown: "Review requested.".to_owned(),
                    resolved_at: None,
                }),
            )
            .unwrap();

        assert_eq!(execution.status, "waiting_human");
        assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
        assert_eq!(
            execution.workspace_path.as_deref(),
            Some("/tmp/mono-agent-001")
        );
        assert!(execution.finished_at.is_none());
        assert_eq!(run.status, "completed");
        assert_eq!(
            run.result_summary.as_deref(),
            Some("Implemented the first pass.")
        );
        assert!(run.error_text.is_none());
        assert!(run.finished_at.is_some());
        let attention = attention.expect("attention item should be created");
        assert_eq!(attention.kind, "review_required");
        assert_eq!(db.list_attention_items(&execution.id).unwrap().len(), 1);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn finishes_active_run_as_failed_and_clears_workspace_when_requested() {
        let path = temp_db_path("finish-run-failed");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();

        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                "/tmp/mono-agent-001",
            )
            .unwrap();

        let (execution, run, attention) = db
            .finish_execution_run(
                &execution.id,
                &run.id,
                "failed",
                "failed",
                None,
                Some("agent run failed"),
                true,
                None,
            )
            .unwrap();

        assert_eq!(execution.status, "failed");
        assert_eq!(execution.cube_repo_id.as_deref(), Some("mono"));
        assert!(execution.cube_lease_id.is_none());
        assert!(execution.workspace_path.is_none());
        assert!(execution.finished_at.is_some());
        assert_eq!(run.status, "failed");
        assert_eq!(run.error_text.as_deref(), Some("agent run failed"));
        assert!(attention.is_none());

        let _ = std::fs::remove_file(path);
    }
}
