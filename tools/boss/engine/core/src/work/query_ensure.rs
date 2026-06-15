use super::*;

/// Extension trait that collapses the pervasive
/// `query_<entity>(conn, id)?.with_context(|| format!("unknown {entity}: {id}"))?`
/// idiom into `query_<entity>(conn, id).require("<entity>", id)?`.
///
/// The `query_*` row helpers in this module return `Result<Option<T>>`
/// — `Ok(None)` means "row not found", `Err` means the query itself
/// failed. `require` turns the `None` case into the canonical
/// `unknown {entity}: {id}` error while propagating any underlying
/// query error verbatim, so the produced messages stay identical to
/// the inline form it replaces.
pub(crate) trait RequireRow<T> {
    fn require(self, entity: &str, id: &str) -> Result<T>;
}

impl<T> RequireRow<T> for Result<Option<T>> {
    fn require(self, entity: &str, id: &str) -> Result<T> {
        self?.with_context(|| format!("unknown {entity}: {id}"))
    }
}

/// Run a `SELECT EXISTS(SELECT 1 FROM ...)` probe and return whether any
/// row matched. Collapses the `query_row(... |row| row.get::<_, i64>(0))
/// != 0` boilerplate that the `ensure_*_exists` bail-checks and the
/// `unique_*_slug` uniqueness loops in this module would otherwise repeat
/// at every site. `sql` must be a single `SELECT EXISTS(...)` expression
/// yielding one `i64` column.
pub(crate) fn row_exists(conn: &Connection, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Result<bool> {
    let exists: i64 = conn.query_row(sql, params, |row| row.get(0))?;
    Ok(exists != 0)
}

/// Resolve the worker branch-name prefix for a new execution from its
/// owning product's `worker_branch_prefix`. Returns `None` (→ engine
/// default `boss/`) when the product carries no override. The stored
/// value is already canonicalised (trailing `/`) at product write
/// time, so it is returned verbatim.
pub(crate) fn resolve_execution_worker_branch_prefix(conn: &Connection, work_item_id: &str) -> Result<Option<String>> {
    let product_id = product_id_for_work_item(conn, work_item_id)?;
    let prefix: Option<String> = conn
        .query_row(
            "SELECT worker_branch_prefix FROM products WHERE id = ?1",
            [&product_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten()
        .filter(|s: &String| !s.is_empty());
    Ok(prefix)
}

/// Resolve the [`BranchNaming`] strategy for a new execution by reading
/// the owning product's `editorial_rules` JSON blob. Falls back to
/// [`BranchNaming::default`] (`BossExecPrefix`) when the product has no
/// rules configured or the JSON does not contain a `branch_naming` key.
/// The resolved value is snapshotted onto the execution row at spawn so
/// it remains stable even if the product rule changes later.
pub(crate) fn resolve_execution_branch_naming(conn: &Connection, work_item_id: &str) -> Result<BranchNaming> {
    let product_id = product_id_for_work_item(conn, work_item_id)?;
    let rules_json: Option<String> = conn
        .query_row(
            "SELECT editorial_rules FROM products WHERE id = ?1",
            [&product_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let naming = rules_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<EditorialRules>(s).ok())
        .map(|r| r.branch_naming)
        .unwrap_or_default();
    Ok(naming)
}

pub(crate) fn query_product(conn: &Connection, id: &str) -> Result<Option<Product>> {
    conn.query_row(
        "SELECT id, name, slug, description, repo_remote_url, status, created_at, updated_at, default_model, dispatch_preamble, external_tracker_kind, external_tracker_config, design_repo, docs_repo, worker_branch_prefix, editorial_rules, default_driver
         FROM products
         WHERE id = ?1",
        [id],
        map_product,
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn query_project(conn: &Connection, id: &str) -> Result<Option<Project>> {
    conn.query_row(
        "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at, last_status_actor,
                design_doc_repo_remote_url, design_doc_branch, design_doc_path, short_id
         FROM projects
         WHERE id = ?1",
        [id],
        map_project,
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn query_task(conn: &Connection, id: &str) -> Result<Option<Task>> {
    conn.query_row(
        "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url, effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id, ci_required_state, review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at, merge_queue_state, driver, parent_task_id
         FROM tasks
         WHERE id = ?1",
        [id],
        map_task_with_parent,
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn query_execution(conn: &Connection, id: &str) -> Result<Option<WorkExecution>> {
    conn.query_row(
        "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                created_at, started_at, finished_at,
                pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count,
                allow_dirty, branch_naming
         FROM work_executions
         WHERE id = ?1",
        [id],
        map_execution,
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn query_run(conn: &Connection, id: &str) -> Result<Option<WorkRun>> {
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

pub(crate) fn query_attention_item(conn: &Connection, id: &str) -> Result<Option<WorkAttentionItem>> {
    conn.query_row(
        "SELECT id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at
         FROM work_attention_items
         WHERE id = ?1",
        [id],
        map_attention_item,
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn list_projects_for_product(conn: &Connection, product_id: &str) -> Result<Vec<Project>> {
    let mut stmt = conn.prepare(
        "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at, last_status_actor,
                design_doc_repo_remote_url, design_doc_branch, design_doc_path, short_id
         FROM projects
         WHERE product_id = ?1
         ORDER BY created_at ASC, name COLLATE NOCASE ASC",
    )?;
    let rows = stmt.query_map([product_id], map_project)?;
    collect_rows(rows)
}

pub(crate) fn list_tasks_for_product(conn: &Connection, product_id: &str) -> Result<Vec<Task>> {
    let mut stmt = conn.prepare(
        "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url, effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id, ci_required_state, review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at, merge_queue_state, driver
         FROM tasks
         WHERE product_id = ?1 AND deleted_at IS NULL
         ORDER BY project_id ASC, ordinal ASC, created_at ASC, id ASC",
    )?;
    let rows = stmt.query_map([product_id], map_task)?;
    collect_rows(rows)
}

pub(crate) fn ensure_product_exists(conn: &Connection, product_id: &str) -> Result<()> {
    if !row_exists(
        conn,
        "SELECT EXISTS(SELECT 1 FROM products WHERE id = ?1)",
        &[&product_id],
    )? {
        bail!("unknown product: {product_id}");
    }
    Ok(())
}

pub(crate) fn ensure_project_exists(conn: &Connection, project_id: &str) -> Result<()> {
    if !row_exists(
        conn,
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1)",
        &[&project_id],
    )? {
        bail!("unknown project: {project_id}");
    }
    Ok(())
}

pub(crate) fn ensure_project_belongs_to_product(conn: &Connection, project_id: &str, product_id: &str) -> Result<()> {
    if !row_exists(
        conn,
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1 AND product_id = ?2)",
        &[&project_id, &product_id],
    )? {
        bail!("project {project_id} does not belong to product {product_id}");
    }
    Ok(())
}
