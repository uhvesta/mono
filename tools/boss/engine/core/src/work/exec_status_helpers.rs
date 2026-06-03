use super::*;

pub(crate) fn execution_kind_for_work_item(
    conn: &Connection,
    work_item_id: &str,
) -> Result<ExecutionKind> {
    Ok(match classify_id(work_item_id)? {
        ItemKind::Product => ExecutionKind::ProductDesign,
        // Project ids no longer host their own executions — the
        // project's design phase lives on its auto-created
        // `kind = 'design'` task. We keep this arm returning
        // `project_design` so legacy callers passing a project id to
        // `RequestExecution` still get a sensible execution kind for
        // logging, but the dispatch loop never actually creates
        // executions against project ids any more.
        ItemKind::Project => ExecutionKind::ProjectDesign,
        ItemKind::Task => {
            let task = query_task(conn, work_item_id)?
                .filter(|task| task.deleted_at.is_none())
                .with_context(|| format!("unknown task: {work_item_id}"))?;
            match task.kind {
                TaskKind::Chore => ExecutionKind::ChoreImplementation,
                TaskKind::Design => ExecutionKind::ProjectDesign,
                TaskKind::Revision => ExecutionKind::RevisionImplementation,
                TaskKind::Investigation => ExecutionKind::InvestigationImplementation,
                TaskKind::ProjectTask | TaskKind::Task => ExecutionKind::TaskImplementation,
            }
        }
    })
}

pub(crate) fn update_execution_status(
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

pub(crate) fn can_reconcile_execution_status(status: &str) -> bool {
    matches!(status, "queued" | "ready" | "waiting_dependency")
}

pub(crate) fn execution_status_is_terminal(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "abandoned" | "cancelled" | "orphaned"
    )
}

/// A *live* execution status: a worker may currently be attached and
/// driving it. Mirrors the status filter in
/// [`query_live_execution_for_work_item`] and
/// [`WorkDb::get_live_execution_for_work_item`]; keep the three in sync.
pub(crate) fn execution_status_is_live(status: &str) -> bool {
    matches!(status, "running" | "waiting_human")
}

pub(crate) fn task_accepts_execution(task: &Task) -> bool {
    if task.deleted_at.is_some() {
        return false;
    }
    // Non-dispatchable states. `in_review` is explicitly blocked here
    // because moving a task directly to `in_review` (e.g. via
    // `boss task update --status in-review` on a task that never went
    // through `active`) used to trigger a spurious worker dispatch: the
    // `UpdateWorkItem` handler calls `publish_work_invalidation` which
    // calls `reconcile_product_executions` for the product, and without
    // this check `reconcile_work_item_execution` would create a `ready`
    // execution for the `in_review` task. The same guard closes the
    // loophole for `archived` and `cancelled` tasks.
    //
    // NOTE: `blocked` is intentionally absent here. Dependency-blocked
    // tasks (status = `blocked`) still need `reconcile_work_item_execution`
    // to run so that `gating_prereqs_for` can create `waiting_dependency`
    // execution rows. The gating logic inside `reconcile_work_item_execution`
    // ensures that dependency-blocked tasks never receive a `ready`/dispatch
    // execution — they only get `waiting_dependency` rows until all
    // prerequisites are complete.
    if matches!(
        task.status.as_str(),
        "done" | "archived" | "cancelled" | "in_review"
    ) {
        return false;
    }
    // Honour the per-task autostart opt-out while the chore/task is
    // still parked in `todo`. The autostart flag is a one-way pause
    // for the auto-dispatcher only — explicit RequestExecution still
    // creates a ready execution. Once `start_execution_run` flips the
    // task to `active` it also clears `autostart` to 0 (single-shot
    // semantics), so `active` tasks always pass this check.
    if !task.autostart && task.status == "todo" {
        return false;
    }
    true
}

pub(crate) fn product_id_for_work_item(conn: &Connection, work_item_id: &str) -> Result<String> {
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

/// Resolve the canonical repo URL for a work item. Reads
/// `tasks.repo_remote_url` first — when set and non-empty, it wins as
/// the per-row override — and otherwise falls back to the parent
/// `products.repo_remote_url`. `None` for both → `Ok(None)` (the
/// caller decides what to do; today's dispatcher will record a
/// `repo_unresolved` attention item per multi-repo Q5).
///
/// For `kind = 'design'` tasks, a non-NULL `products.design_repo`
/// takes precedence over `products.repo_remote_url` at the product
/// layer — the per-row task override still wins, so this slots in as
/// a new middle layer between the row override and the product
/// default. Implementation kinds (`task`, `chore`, `project_task`)
/// are unaffected.
///
/// No project layer: projects don't carry their own override (Q2),
/// they inherit transitively through their tasks. A non-task
/// `work_item_id` therefore returns `Ok(None)` since project / product
/// rows don't dispatch on their own.
///
/// Errors only when the task row references a `product_id` that is no
/// longer in the products table (an orphan task — a referential-
/// integrity break the caller should surface, not paper over with a
/// silent fallback).
///
/// This is the single resolution point per the multi-repo design's R1
/// mitigation: every dispatch and listing surface must route through
/// this helper so the rule never diverges.
pub(crate) fn resolve_repo_for_work_item(
    conn: &Connection,
    work_item_id: &str,
) -> Result<Option<String>> {
    let row: Option<(Option<String>, String, String)> = conn
        .query_row(
            "SELECT repo_remote_url, product_id, kind FROM tasks WHERE id = ?1",
            [work_item_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;

    let Some((override_repo, product_id, kind_str)) = row else {
        return Ok(None);
    };
    let kind: TaskKind = kind_str
        .parse()
        .map_err(|_| anyhow::anyhow!("unknown task kind in DB row {work_item_id}: {kind_str}"))?;

    if let Some(url) = override_repo.as_deref().filter(|s| !s.is_empty()) {
        return Ok(Some(url.to_owned()));
    }

    let product = query_product(conn, &product_id)?.with_context(|| {
        format!("orphan task {work_item_id}: parent product {product_id} missing")
    })?;
    match kind {
        TaskKind::Design => {
            if let Some(url) = product.design_repo.as_deref().filter(|s| !s.is_empty()) {
                return Ok(Some(url.to_owned()));
            }
        }
        TaskKind::Investigation => {
            // Investigation deliverables go to the product's docs_repo; if
            // unset the worker falls back to BOSS_USER_DOCS_REPO (resolved
            // at spawn time by the dispatcher). Returning None here is
            // intentional: the dispatcher's repo-unresolved attention item
            // path surfaces it to the coordinator for manual correction.
            if let Some(url) = product.docs_repo.as_deref().filter(|s| !s.is_empty()) {
                return Ok(Some(url.to_owned()));
            }
            if let Ok(user_docs) = std::env::var("BOSS_USER_DOCS_REPO") {
                if !user_docs.is_empty() {
                    return Ok(Some(user_docs));
                }
            }
        }
        // All other kinds use the product's default code repo.
        TaskKind::Chore | TaskKind::ProjectTask | TaskKind::Revision | TaskKind::Task => {}
    }
    Ok(product.repo_remote_url)
}

pub(crate) fn resolve_execution_repo_remote_url(
    conn: &Connection,
    work_item_id: &str,
    explicit_repo_remote_url: Option<String>,
) -> Result<String> {
    if let Some(repo_remote_url) = explicit_repo_remote_url {
        let _ = product_id_for_work_item(conn, work_item_id)?;
        return Ok(repo_remote_url);
    }

    // Multi-repo Q5: route through the single resolver so per-row
    // overrides on `tasks.repo_remote_url` beat the product default.
    // Errors keep the same shape the bossctl path expects.
    resolve_repo_for_work_item(conn, work_item_id)?.with_context(|| {
        format!(
            "work item {work_item_id} does not resolve to a repo_remote_url; provide one explicitly"
        )
    })
}

pub(crate) fn next_task_ordinal(conn: &Connection, project_id: &str) -> Result<i64> {
    let current = conn.query_row(
        "SELECT COALESCE(MAX(ordinal), 0) FROM tasks
             WHERE project_id = ?1 AND kind = 'project_task' AND deleted_at IS NULL",
        [project_id],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(current + 1)
}

pub(crate) fn unique_product_slug(conn: &Connection, base_slug: &str) -> Result<String> {
    let base_slug = default_slug(base_slug);
    let mut candidate = base_slug.clone();
    let mut suffix = 2;
    while row_exists(
        conn,
        "SELECT EXISTS(SELECT 1 FROM products WHERE slug = ?1)",
        &[&candidate],
    )? {
        candidate = format!("{base_slug}-{suffix}");
        suffix += 1;
    }
    Ok(candidate)
}

pub(crate) fn unique_product_slug_for_update(
    conn: &Connection,
    id: &str,
    base_slug: &str,
) -> Result<String> {
    let base_slug = default_slug(base_slug);
    let mut candidate = base_slug.clone();
    let mut suffix = 2;
    while row_exists(
        conn,
        "SELECT EXISTS(SELECT 1 FROM products WHERE slug = ?1 AND id != ?2)",
        &[&candidate, &id],
    )? {
        candidate = format!("{base_slug}-{suffix}");
        suffix += 1;
    }
    Ok(candidate)
}

pub(crate) fn unique_project_slug(
    conn: &Connection,
    product_id: &str,
    base_slug: &str,
) -> Result<String> {
    let base_slug = default_slug(base_slug);
    let mut candidate = base_slug.clone();
    let mut suffix = 2;
    while row_exists(
        conn,
        "SELECT EXISTS(SELECT 1 FROM projects WHERE product_id = ?1 AND slug = ?2)",
        &[&product_id, &candidate],
    )? {
        candidate = format!("{base_slug}-{suffix}");
        suffix += 1;
    }
    Ok(candidate)
}

pub(crate) fn unique_project_slug_for_update(
    conn: &Connection,
    product_id: &str,
    id: &str,
    base_slug: &str,
) -> Result<String> {
    let base_slug = default_slug(base_slug);
    let mut candidate = base_slug.clone();
    let mut suffix = 2;
    while row_exists(
        conn,
        "SELECT EXISTS(SELECT 1 FROM projects WHERE product_id = ?1 AND slug = ?2 AND id != ?3)",
        &[&product_id, &candidate, &id],
    )? {
        candidate = format!("{base_slug}-{suffix}");
        suffix += 1;
    }
    Ok(candidate)
}

pub(crate) fn default_slug(base_slug: &str) -> String {
    if base_slug.is_empty() {
        "item".to_owned()
    } else {
        base_slug.to_owned()
    }
}
