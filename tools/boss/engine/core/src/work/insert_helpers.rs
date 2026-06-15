use super::*;

/// Upsert the multi-signal side table for a `(work_item_id, reason)`
/// pair. The PK collapses repeat observations to one row; we reset
/// `cleared_at` to NULL on re-observation so the same signal flapping
/// in and out lands as one row with the latest `created_at`.
///
/// `attempt_id` is the soft FK that the design's §Q2 stores so the UI
/// can navigate from a signal back to its attempt row; `None` for
/// `'dependency'` (which has no attempt table) and for the
/// `'ci_failure_exhausted'` signal (which is the *absence* of an
/// engine-managed attempt — the engine has stopped trying).
/// Insert a `ci_failure_suppressions` row for the work item, keyed by
/// the head sha of the most recent `ci_remediations` attempt. Called
/// from `update_task` when a human moves a chore out of `blocked:
/// ci_failure` (or `ci_failure_exhausted`) — see design §Q5 ("Manual
/// override (CI)") and Phase 12 #38. The function is best-effort:
/// when no `ci_remediations` row exists (the chore was manually moved
/// without the engine having ever recorded an attempt — e.g. a budget=0
/// `notify only` flow) we resort to the `tasks.pr_url` value combined
/// with a sentinel head sha so the suppression still keys to a real
/// head if the chore has one; if even that is missing, we leave the
/// table alone — the engine has no head sha to suppress against and
/// the next probe will simply re-observe the failure.
///
/// We also reset `ci_attempts_used` so the next CI failure (on a new
/// head sha — the suppression has expired by then) starts with a
/// fresh budget; mirrors the manual `boss engine ci retry` reset rule.
pub(crate) fn record_ci_failure_suppression_in_tx(conn: &Connection, work_item_id: &str, now: &str) -> Result<()> {
    // The most recent `ci_remediations` row carries the head sha the
    // engine was reacting to. Prefer the latest attempt regardless of
    // status — the user may be moving off `ci_failure_exhausted`, in
    // which case the row is terminal but its head sha is still what
    // we should suppress.
    let head_sha: Option<String> = conn
        .query_row(
            "SELECT head_sha_at_trigger FROM ci_remediations
              WHERE work_item_id = ?1
              ORDER BY created_at DESC, id DESC
              LIMIT 1",
            params![work_item_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(head_sha) = head_sha {
        conn.execute(
            "INSERT OR REPLACE INTO ci_failure_suppressions
                 (work_item_id, head_sha, created_at)
             VALUES (?1, ?2, ?3)",
            params![work_item_id, head_sha, now],
        )?;
    } else {
        tracing::debug!(
            work_item_id,
            "record_ci_failure_suppression_in_tx: no ci_remediations row; skipping suppression insert",
        );
    }
    // Reset the per-PR budget so a future fresh-head failure starts
    // clean. The reset is unconditional within this code path —
    // pulling a row out of `ci_failure` is itself an override of the
    // budget logic.
    conn.execute(
        "UPDATE tasks
            SET ci_attempts_used = 0
          WHERE id = ?1
            AND deleted_at IS NULL",
        params![work_item_id],
    )?;
    Ok(())
}

pub(crate) fn upsert_task_blocked_signal(
    conn: &Connection,
    work_item_id: &str,
    reason: &str,
    attempt_id: Option<&str>,
    now: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO task_blocked_signals
             (work_item_id, reason, attempt_id, created_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(work_item_id, reason) DO UPDATE SET
             attempt_id = COALESCE(excluded.attempt_id, task_blocked_signals.attempt_id),
             cleared_at = NULL",
        params![work_item_id, reason, attempt_id, now],
    )?;
    Ok(())
}

/// Check whether a non-deleted task/chore with the same trimmed name
/// exists in the same product and was created within `DUPLICATE_GUARD_WINDOW_SECS`.
/// Returns `Some(DuplicateTaskError)` when the guard fires, `None` otherwise.
pub(crate) fn check_recent_duplicate(
    conn: &Connection,
    product_id: &str,
    name: &str,
) -> Result<Option<DuplicateTaskError>> {
    let trimmed = name.trim();
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let cutoff = now_secs - DUPLICATE_GUARD_WINDOW_SECS;

    let row: Option<(String, Option<i64>, i64)> = conn
        .query_row(
            "SELECT id, short_id, CAST(created_at AS INTEGER)
             FROM tasks
             WHERE product_id = ?1
               AND trim(name) = ?2
               AND deleted_at IS NULL
               AND CAST(created_at AS INTEGER) >= ?3
             ORDER BY CAST(created_at AS INTEGER) DESC
             LIMIT 1",
            params![product_id, trimmed, cutoff],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;

    Ok(
        row.map(|(existing_id, existing_short_id, created_at)| DuplicateTaskError {
            existing_id,
            existing_short_id: existing_short_id.unwrap_or(0),
            name: trimmed.to_owned(),
            age_secs: now_secs - created_at,
        }),
    )
}

pub(crate) fn insert_task_in_tx(conn: &Connection, input: CreateTaskInput) -> Result<Task> {
    ensure_product_exists(conn, &input.product_id)?;
    ensure_project_belongs_to_product(conn, &input.project_id, &input.product_id)?;

    if !input.force_duplicate
        && let Some(dup) = check_recent_duplicate(conn, &input.product_id, &input.name)?
    {
        return Err(anyhow::Error::new(dup));
    }

    let product = query_product(conn, &input.product_id)?
        .with_context(|| format!("missing product after existence check: {}", input.product_id))?;
    let id = next_id("task");
    let now = now_string();
    let ordinal = next_task_ordinal(conn, &input.project_id)?;
    let description = input.description.unwrap_or_default();
    let autostart_value: i64 = if input.autostart { 1 } else { 0 };
    let priority = normalize_priority(input.priority.as_deref())?;
    let created_via = canonicalize_created_via(input.created_via.as_deref(), &id, "task");
    let repo_remote_url = enforce_task_repo_invariant(&product, input.repo_remote_url)?;
    let effort_level = input.effort_level.map(|level| level.as_str().to_owned());
    let model_override = normalize_model_override(input.model_override);
    let driver = normalize_model_override(input.driver);
    let short_id = allocate_short_id(conn, &input.product_id)?;

    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, repo_remote_url, effort_level, model_override, driver, short_id)
         VALUES (?1, ?2, ?3, 'project_task', ?4, ?5, 'todo', ?6, NULL, NULL, ?7, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![id, input.product_id, input.project_id, input.name, description, ordinal, now, autostart_value, priority, created_via, repo_remote_url, effort_level, model_override, driver, short_id],
    )?;

    query_task(conn, &id)?.with_context(|| format!("missing task after insert: {id}"))
}

pub(crate) fn insert_chore_in_tx(conn: &Connection, input: CreateChoreInput) -> Result<Task> {
    ensure_product_exists(conn, &input.product_id)?;

    if !input.force_duplicate
        && let Some(dup) = check_recent_duplicate(conn, &input.product_id, &input.name)?
    {
        return Err(anyhow::Error::new(dup));
    }

    let product = query_product(conn, &input.product_id)?
        .with_context(|| format!("missing product after existence check: {}", input.product_id))?;
    let id = next_id("task");
    let now = now_string();
    let description = input.description.unwrap_or_default();
    let autostart_value: i64 = if input.autostart { 1 } else { 0 };
    let priority = normalize_priority(input.priority.as_deref())?;
    let kind_str = input.kind_override.as_ref().map(|k| k.as_str()).unwrap_or("chore");
    let created_via = canonicalize_created_via(input.created_via.as_deref(), &id, kind_str);
    let repo_remote_url = enforce_task_repo_invariant(&product, input.repo_remote_url)?;
    let effort_level = input.effort_level.map(|level| level.as_str().to_owned());
    let model_override = normalize_model_override(input.model_override);
    let driver = normalize_model_override(input.driver);
    let short_id = allocate_short_id(conn, &input.product_id)?;

    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, repo_remote_url, effort_level, model_override, driver, short_id, origin_task_short_id, origin_pr_number)
         VALUES (?1, ?2, NULL, ?3, ?4, ?5, 'todo', NULL, NULL, NULL, ?6, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![id, input.product_id, kind_str, input.name, description, now, autostart_value, priority, created_via, repo_remote_url, effort_level, model_override, driver, short_id, input.origin_task_short_id, input.origin_pr_number],
    )?;

    query_task(conn, &id)?.with_context(|| format!("missing chore after insert: {id}"))
}

/// Insert a `kind = 'investigation'` task. Mirrors `insert_chore_in_tx`
/// but uses `investigation` kind and accepts an optional `project_id`.
/// The repo stored on the task row is the investigation deliverable repo
/// (product `docs_repo` or `BOSS_USER_DOCS_REPO`), not the product's
/// code repo — `enforce_task_repo_invariant` is NOT called so the
/// override can point at a docs-only repo without triggering the
/// same-product check.
pub(crate) fn insert_investigation_in_tx(
    conn: &Connection,
    input: boss_protocol::CreateInvestigationInput,
) -> Result<Task> {
    ensure_product_exists(conn, &input.product_id)?;
    if let Some(ref pid) = input.project_id {
        ensure_project_belongs_to_product(conn, pid, &input.product_id)?;
    }
    if !input.force_duplicate
        && let Some(dup) = check_recent_duplicate(conn, &input.product_id, &input.name)?
    {
        return Err(anyhow::Error::new(dup));
    }
    let id = next_id("task");
    let now = now_string();
    let description = input.description.unwrap_or_default();
    let autostart_value: i64 = if input.autostart { 1 } else { 0 };
    let priority = normalize_priority(input.priority.as_deref())?;
    let created_via = canonicalize_created_via(input.created_via.as_deref(), &id, "investigation");
    let repo_remote_url = input.repo_remote_url.filter(|s| !s.is_empty());
    let effort_level = input.effort_level.map(|level| level.as_str().to_owned());
    let model_override = normalize_model_override(input.model_override);
    let driver = normalize_model_override(input.driver);
    let short_id = allocate_short_id(conn, &input.product_id)?;
    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, repo_remote_url, effort_level, model_override, driver, short_id)
         VALUES (?1, ?2, ?3, 'investigation', ?4, ?5, 'todo', NULL, NULL, NULL, ?6, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            id, input.product_id, input.project_id, input.name, description, now,
            autostart_value, priority, created_via, repo_remote_url,
            effort_level, model_override, driver, short_id
        ],
    )?;
    query_task(conn, &id)?.with_context(|| format!("missing investigation after insert: {id}"))
}
