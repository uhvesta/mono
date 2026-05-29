use super::*;

/// Allocate the next per-product `short_id` for a new `tasks` or
/// `projects` row. Reads the current `next_value` from
/// `short_id_sequences` for `product_id`, defaulting to 1 if no row
/// exists yet, writes back `next_value + 1`, and returns the value
/// just claimed. Must be called inside the same SQLite transaction as
/// the row insert; SQLite serialises writers in WAL mode, so two
/// concurrent inserts against the same product receive distinct ids.
///
/// See `tools/boss/docs/designs/friendly-numeric-ids-for-work-items.md`
/// (Q3) for the reasoning behind the per-product scope and the
/// in-transaction read-modify-write pattern.
pub(crate) fn allocate_short_id(conn: &Connection, product_id: &str) -> Result<i64> {
    let current: i64 = conn
        .query_row(
            "SELECT next_value FROM short_id_sequences WHERE product_id = ?1",
            [product_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(1);
    conn.execute(
        "INSERT INTO short_id_sequences(product_id, next_value) VALUES(?1, ?2)
         ON CONFLICT(product_id) DO UPDATE SET next_value = excluded.next_value",
        params![product_id, current + 1],
    )?;
    Ok(current)
}

/// Parallel to [`allocate_short_id`] for the `A<n>` automation namespace.
/// Reads and advances `automation_short_id_sequences` for `product_id`.
/// Must be called inside the same transaction as the `automations` row insert.
/// See `tools/boss/docs/designs/maintenance-tasks.md` §"Short-id namespace".
pub(crate) fn allocate_automation_short_id(conn: &Connection, product_id: &str) -> Result<i64> {
    let current: i64 = conn
        .query_row(
            "SELECT next_value FROM automation_short_id_sequences WHERE product_id = ?1",
            [product_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(1);
    conn.execute(
        "INSERT INTO automation_short_id_sequences(product_id, next_value) VALUES(?1, ?2)
         ON CONFLICT(product_id) DO UPDATE SET next_value = excluded.next_value",
        params![product_id, current + 1],
    )?;
    Ok(current)
}

/// Validate the `(execution_id, work_item_id)` discriminant on a
/// `CreateAttentionItemInput` and return the canonical pair to write.
/// Exactly one of the two must be set; both-set or neither-set is a
/// caller bug. Also confirms the referenced row actually exists so
/// the CHECK constraint and FK don't blow up on insert.
pub(crate) fn attention_target_from_input(
    conn: &Connection,
    input: &CreateAttentionItemInput,
) -> Result<(Option<String>, Option<String>)> {
    let exec = input.execution_id.as_deref().filter(|s| !s.is_empty());
    let work = input.work_item_id.as_deref().filter(|s| !s.is_empty());
    match (exec, work) {
        (Some(execution_id), None) => {
            ensure_execution_exists(conn, execution_id)?;
            Ok((Some(execution_id.to_owned()), None))
        }
        (None, Some(work_item_id)) => {
            let _ = product_id_for_work_item(conn, work_item_id)?;
            Ok((None, Some(work_item_id.to_owned())))
        }
        (Some(_), Some(_)) => {
            bail!("attention item must reference either execution_id or work_item_id, not both")
        }
        (None, None) => bail!("attention item must reference either execution_id or work_item_id"),
    }
}

/// Emit a sticky `repo_unresolved` attention item against
/// `work_item_id`, unless one is already open. Idempotent: repeated
/// reconcile passes against the same work item don't pile up rows.
/// Caller supplies the kind label (`task`, `chore`, `project`) so
/// the message names the right CLI verb.
pub(crate) fn record_repo_unresolved_attention(
    conn: &Connection,
    work_item_id: &str,
    kind_label: &str,
) -> Result<()> {
    let already_open: i64 = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM work_attention_items
             WHERE work_item_id = ?1
               AND kind = 'repo_unresolved'
               AND status = 'open'
         )",
        [work_item_id],
        |row| row.get(0),
    )?;
    if already_open != 0 {
        return Ok(());
    }
    let id = next_id("attn");
    let now = now_string();
    let title = format!("Work item {work_item_id} has no repo resolution");
    let body = repo_unresolved_attention_body(work_item_id, kind_label);
    conn.execute(
        "INSERT INTO work_attention_items (
            id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at
         ) VALUES (?1, NULL, ?2, 'repo_unresolved', 'open', ?3, ?4, ?5, NULL)",
        params![id, work_item_id, title, body, now],
    )?;
    Ok(())
}

/// Shared precheck for any dispatch trigger (request-execution,
/// kanban drag-to-Doing). Returns `Ok(())` when the work item
/// resolves to a repo URL. When it doesn't, writes a sticky
/// `repo_unresolved` attention item via a short-lived transaction
/// (so the row commits before the caller's bail unwinds anything
/// else) and bails with the same human-facing message
/// `repo_unresolved_attention_body` produces. Callers MUST resolve
/// friendly ids (`T42`) before passing `work_item_id` here.
pub(crate) fn ensure_dispatch_repo_resolvable(
    conn: &mut Connection,
    work_item_id: &str,
) -> Result<()> {
    if resolve_repo_for_work_item(conn, work_item_id)?.is_some() {
        return Ok(());
    }
    let label = repo_unresolved_kind_label(conn, work_item_id)?;
    let attn_tx = conn.transaction()?;
    record_repo_unresolved_attention(&attn_tx, work_item_id, label)?;
    attn_tx.commit()?;
    bail!("{}", repo_unresolved_attention_body(work_item_id, label));
}

/// The exact message text both the attention item and the
/// `request_execution` bail path use. Single source so the two
/// surfaces never drift, per the design doc's R1 mitigation.
pub(crate) fn repo_unresolved_attention_body(work_item_id: &str, kind_label: &str) -> String {
    format!(
        "work item {work_item_id} has no repo resolution; set one with `boss {kind_label} update --repo <url>` or set a product default."
    )
}

/// Kind label for the `boss <kind> update` hint in the
/// `repo_unresolved` message. Tasks under a project use `task`;
/// project-less rows are `chore`. Projects don't dispatch directly,
/// so the message there falls back to the safe generic.
pub(crate) fn repo_unresolved_kind_label(
    conn: &Connection,
    work_item_id: &str,
) -> Result<&'static str> {
    Ok(match classify_id(work_item_id)? {
        ItemKind::Task => {
            let task = query_task(conn, work_item_id)?
                .filter(|task| task.deleted_at.is_none())
                .with_context(|| format!("unknown task: {work_item_id}"))?;
            match task.kind.as_str() {
                "chore" => "chore",
                _ => "task",
            }
        }
        ItemKind::Project => "project",
        ItemKind::Product => "product",
    })
}

pub(crate) fn ensure_execution_exists(conn: &Connection, execution_id: &str) -> Result<()> {
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

/// Edges where the dependent belongs to `product_id`. Joins
/// `work_item_dependencies` against `tasks` (live rows only) and
/// `projects` so cross-product or stale-by-deletion edges never leak
/// into a kanban payload. Sorted to match `prerequisites_of` /
/// `dependents_of` so consumers see a stable order.
pub(crate) fn collect_product_dependencies(
    conn: &Connection,
    product_id: &str,
) -> Result<Vec<WorkItemDependency>> {
    let mut stmt = conn.prepare(
        "SELECT d.dependent_id, d.prerequisite_id, d.relation, d.created_at
         FROM work_item_dependencies d
         WHERE EXISTS (
             SELECT 1 FROM tasks t
             WHERE t.id = d.dependent_id
               AND t.product_id = ?1
               AND t.deleted_at IS NULL
         )
         OR EXISTS (
             SELECT 1 FROM projects p
             WHERE p.id = d.dependent_id
               AND p.product_id = ?1
         )
         ORDER BY d.created_at ASC, d.dependent_id ASC, d.prerequisite_id ASC",
    )?;
    let rows = stmt.query_map([product_id], |row| {
        Ok(WorkItemDependency {
            dependent_id: row.get(0)?,
            prerequisite_id: row.get(1)?,
            relation: row.get(2)?,
            created_at: row.get(3)?,
        })
    })?;
    collect_rows(rows)
}

pub(crate) fn collect_task_runtimes(
    conn: &Connection,
    tasks: &[Task],
    chores: &[Task],
) -> Result<Vec<TaskRuntime>> {
    let mut runtimes = Vec::with_capacity(tasks.len() + chores.len());
    for task in tasks.iter().chain(chores.iter()) {
        runtimes.push(query_task_runtime(conn, &task.id)?);
    }
    Ok(runtimes)
}

pub(crate) fn query_task_runtime(conn: &Connection, work_item_id: &str) -> Result<TaskRuntime> {
    let latest = query_latest_execution_for_work_item(conn, work_item_id)?;
    // `current_execution_id` (the operator-facing label for
    // `TaskRuntime.execution_id`) and the kanban card must follow the
    // execution a worker is actually attached to. A re-dispatch storm
    // leaves a newer *terminal* execution (the stalled duplicate)
    // shadowing the live run — keying off the plain latest row then
    // detaches the card from the live worker (R693 showed up idle under
    // "No Project" while La Forge was actively working it). When the
    // latest row is not itself live, prefer a live (running /
    // waiting_human) execution. Steady state — the latest row IS the
    // live run — skips the extra lookup.
    let latest_is_live = latest
        .as_ref()
        .map(|e| execution_status_is_live(&e.status))
        .unwrap_or(false);
    let execution = if latest_is_live {
        latest
    } else if let Some(live) = query_live_execution_for_work_item(conn, work_item_id)? {
        Some(live)
    } else {
        latest
    };
    let (execution_status, run_status, execution_id, current_run_id) =
        if let Some(execution) = execution {
            let latest_run = query_latest_run(conn, &execution.id)?;
            let (run_status, run_id) = match latest_run {
                Some((id, status)) => (Some(status), Some(id)),
                None => (None, None),
            };
            (
                Some(execution.status),
                run_status,
                Some(execution.id),
                run_id,
            )
        } else {
            (None, None, None, None)
        };
    Ok(TaskRuntime {
        work_item_id: work_item_id.to_owned(),
        execution_status,
        run_status,
        execution_id,
        current_run_id,
    })
}

pub(crate) fn query_latest_run(
    conn: &Connection,
    execution_id: &str,
) -> Result<Option<(String, String)>> {
    conn.query_row(
        "SELECT id, status
         FROM work_runs
         WHERE execution_id = ?1
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
        [execution_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn query_latest_execution_for_work_item(
    conn: &Connection,
    work_item_id: &str,
) -> Result<Option<WorkExecution>> {
    conn.query_row(
        "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                created_at, started_at, finished_at,
                pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count
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

/// Most-recent execution for `work_item_id` whose DB status is *live* —
/// a worker may currently be attached. Unlike
/// [`query_latest_execution_for_work_item`], the result is NOT shadowed
/// by a newer terminal row: a re-dispatch storm produces stalled
/// duplicates that get abandoned/orphaned (terminal) ON TOP of the one
/// genuinely-live run, so "latest by created_at" points at the phantom
/// while the live execution sits one row down. Callers that need to
/// answer "is this work item already being worked?" must key off this,
/// not the latest row. Mirrors the status set of the method
/// [`WorkDb::get_live_execution_for_work_item`].
pub(crate) fn query_live_execution_for_work_item(
    conn: &Connection,
    work_item_id: &str,
) -> Result<Option<WorkExecution>> {
    conn.query_row(
        "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                created_at, started_at, finished_at,
                pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count
         FROM work_executions
         WHERE work_item_id = ?1
           AND status IN ('running', 'waiting_human')
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
        [work_item_id],
        map_execution,
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn reconcile_work_item_execution(
    conn: &Connection,
    result: &mut ExecutionReconcileResult,
    work_item_id: &str,
    kind: &str,
    desired_status: &str,
) -> Result<()> {
    // Dispatcher gate (Q8): if the work item has any unmet `blocks`
    // prereq, downgrade its desired execution status to
    // `waiting_dependency` regardless of what the caller asked for.
    // This keeps gated dependents out of `ready` and therefore out
    // of the dispatcher's pickup pool.
    let gated = !deps::gating_prereqs_for(conn, work_item_id)?.is_empty();
    let effective_status = if gated && desired_status == "ready" {
        "waiting_dependency"
    } else {
        desired_status
    };
    match query_latest_execution_for_work_item(conn, work_item_id)? {
        Some(execution) => {
            if execution.kind == kind
                && can_reconcile_execution_status(&execution.status)
                && execution.status != effective_status
            {
                let updated = update_execution_status(conn, &execution.id, effective_status)?;
                result.updated.push(updated);
            }
        }
        None => {
            // Resolve through the single helper so per-row overrides
            // beat the product default (multi-repo design Q5). On a
            // `None` we don't create an execution row — instead a
            // sticky `repo_unresolved` attention item surfaces the
            // problem in the kanban Attention lane.
            let Some(repo_remote_url) = resolve_repo_for_work_item(conn, work_item_id)? else {
                let label = repo_unresolved_kind_label(conn, work_item_id)?;
                record_repo_unresolved_attention(conn, work_item_id, label)?;
                return Ok(());
            };
            let created = insert_execution(
                conn,
                CreateExecutionInput {
                    work_item_id: work_item_id.to_owned(),
                    kind: kind.to_owned(),
                    status: Some(effective_status.to_owned()),
                    repo_remote_url: Some(repo_remote_url),
                    cube_repo_id: None,
                    cube_lease_id: None,
                    cube_workspace_id: None,
                    workspace_path: None,
                    priority: None,
                    preferred_workspace_id: None,
                    started_at: None,
                    finished_at: None,
                    prefer_is_soft: false,
                    pr_url: None,
                },
            )?;
            result.created.push(created);
        }
    }

    Ok(())
}

/// Look up the chain root's task for a revision (the first non-revision
/// ancestor) and return it. Returns `None` if the chain root can't be
/// resolved (broken parent link or missing task).
pub(crate) fn get_chain_root_task(conn: &Connection, revision_id: &str) -> Result<Option<Task>> {
    let root_id = chain_root(conn, revision_id)?;
    if root_id == revision_id {
        // chain_root didn't walk anywhere — either the task itself is the
        // chain root (non-revision) or the parent link is missing. In
        // both cases, there is no real parent PR to revise; skip.
        return Ok(None);
    }
    query_task(conn, &root_id)
}

/// Return the `cube_workspace_id` from the most recent non-failed
/// execution of `chain_root_id`. Used as the soft preferred workspace
/// for revision dispatch — warmth only, never a hard requirement.
pub(crate) fn preferred_workspace_for_chain_root(
    conn: &Connection,
    chain_root_id: &str,
) -> Result<Option<String>> {
    conn.query_row(
        "SELECT cube_workspace_id
         FROM work_executions
         WHERE work_item_id = ?1
           AND status NOT IN ('failed', 'cancelled', 'orphaned', 'abandoned')
           AND cube_workspace_id IS NOT NULL
           AND cube_workspace_id != ''
         ORDER BY created_at DESC
         LIMIT 1",
        [chain_root_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .optional()
    .map(|opt| opt.flatten())
    .map_err(Into::into)
}

/// Dispatch arm for `kind = 'revision'` tasks.
///
/// Dispatch-time cached gate: if the chain root is already `done` (PR
/// merged or closed before this reconcile tick), move the revision to
/// `blocked` with a clear reason and surface a `WorkAttentionItem`.
/// This catches the common case; the coordinator adds a live probe
/// for the race window between poller ticks.
///
/// If the gate passes, create a `revision_implementation` execution
/// with `prefer_is_soft = true` (soft cube-workspace preference) and
/// `pr_url` set to the chain root's PR URL (so the SHA-delta gate can
/// snapshot the parent PR's HEAD and detect when the revision worker
/// contributes).
pub(crate) fn reconcile_revision_execution(
    conn: &Connection,
    result: &mut ExecutionReconcileResult,
    task: &Task,
) -> Result<()> {
    // Walk to the chain root.
    let chain_root_task = match get_chain_root_task(conn, &task.id)? {
        Some(t) => t,
        None => {
            // Broken parent link — the revision has no resolvable chain
            // root.  Skip dispatch; this is surfaced elsewhere as a data
            // integrity problem.
            tracing::warn!(
                task_id = %task.id,
                "reconcile_revision: cannot resolve chain root; skipping dispatch",
            );
            return Ok(());
        }
    };

    // Chain root must have an open PR for the revision to push to.
    let parent_pr_url = match chain_root_task.pr_url.as_deref().filter(|u| !u.is_empty()) {
        Some(u) => u.to_owned(),
        None => {
            // No PR yet — the parent hasn't been dispatched or hasn't
            // opened a PR.  Stay in `todo` until the parent creates one.
            tracing::debug!(
                task_id = %task.id,
                chain_root_id = %chain_root_task.id,
                "reconcile_revision: chain root has no pr_url yet; deferring revision dispatch",
            );
            return Ok(());
        }
    };

    // Cached dispatch-time gate: if the chain root is `done`, the PR has
    // merged or been closed.  Auto-block the revision so it doesn't get
    // dispatched into an already-merged branch.
    if chain_root_task.status == "done" || chain_root_task.status == "archived" {
        let now = now_string();
        let rows_changed = conn.execute(
            "UPDATE tasks
             SET status = 'blocked',
                 blocked_reason = 'parent_pr_closed',
                 last_status_actor = 'engine',
                 updated_at = ?2
             WHERE id = ?1
               AND status NOT IN ('blocked', 'done', 'archived')
               AND deleted_at IS NULL",
            params![task.id, now],
        )?;
        if rows_changed > 0 {
            tracing::info!(
                task_id = %task.id,
                chain_root_id = %chain_root_task.id,
                "reconcile_revision: chain root is done; blocked revision (parent PR closed/merged)",
            );
            // Surface as an attention item on the revision task.
            let attn_id = next_id("attn");
            conn.execute(
                "INSERT INTO work_attention_items
                     (id, execution_id, work_item_id, kind, status, title, body_markdown, created_at)
                 VALUES (?1, NULL, ?2, 'revision_parent_closed', 'open',
                         'Parent PR closed before revision dispatched',
                         'The parent task''s PR was merged or closed before this revision could be dispatched. File a new task if further changes are needed.',
                         ?3)",
                params![attn_id, task.id, now],
            )?;
        }
        return Ok(());
    }

    // Gate passed.  Create or refresh the execution row.
    let gated = !deps::gating_prereqs_for(conn, &task.id)?.is_empty();
    let effective_status = if gated { "waiting_dependency" } else { "ready" };

    match query_latest_execution_for_work_item(conn, &task.id)? {
        Some(existing)
            if existing.kind == "revision_implementation"
                && can_reconcile_execution_status(&existing.status)
                && existing.status != effective_status =>
        {
            let updated = update_execution_status(conn, &existing.id, effective_status)?;
            result.updated.push(updated);
        }
        Some(existing)
            if existing.kind == "revision_implementation"
                && can_reconcile_execution_status(&existing.status) =>
        {
            // Already in the right status — nothing to do.
        }
        _ => {
            // No matching execution yet (or previous is terminal) — create one.
            let Some(repo_remote_url) = resolve_repo_for_work_item(conn, &task.id)? else {
                let label = repo_unresolved_kind_label(conn, &task.id)?;
                record_repo_unresolved_attention(conn, &task.id, label)?;
                return Ok(());
            };
            let preferred_workspace_id =
                preferred_workspace_for_chain_root(conn, &chain_root_task.id)?;
            let created = insert_execution(
                conn,
                CreateExecutionInput {
                    work_item_id: task.id.clone(),
                    kind: "revision_implementation".to_owned(),
                    status: Some(effective_status.to_owned()),
                    repo_remote_url: Some(repo_remote_url),
                    cube_repo_id: None,
                    cube_lease_id: None,
                    cube_workspace_id: None,
                    workspace_path: None,
                    priority: None,
                    preferred_workspace_id,
                    started_at: None,
                    finished_at: None,
                    prefer_is_soft: true,
                    pr_url: Some(parent_pr_url),
                },
            )?;
            result.created.push(created);
        }
    }
    Ok(())
}

pub(crate) fn request_execution_in_tx_with_live_check<F: FnOnce(&str) -> bool>(
    conn: &Connection,
    input: RequestExecutionInput,
    is_live: F,
) -> Result<WorkExecution> {
    let RequestExecutionInput {
        work_item_id,
        priority,
        preferred_workspace_id,
        // `force` is purely a dispatcher hint (handled by
        // `ExecutionCoordinator::force_dispatch`); the DB layer just
        // creates / refreshes a `ready` row the same way for both
        // forced and queued requests.
        force: _,
    } = input;

    let preferred_workspace_id = normalize_optional_text(preferred_workspace_id);
    let kind = execution_kind_for_work_item(conn, &work_item_id)?;

    // Q8: explicit `RequestExecution` against a gated work item is
    // refused with a clear error rather than silently overridden. A
    // future `--force` may relax this; for v1, the user removes the
    // edge or waits for the prereq to land.
    let gating = deps::gating_prereqs_for(conn, &work_item_id)?;
    if !gating.is_empty() {
        let names = gating.join(", ");
        bail!(
            "cannot start {work_item_id}: gated by [{names}] — use `boss <kind> depend rm` to remove the edge or wait for the prereq to complete"
        );
    }

    // Prereqs are all satisfied. If the task is stuck in `blocked` with
    // blocked_reason='dependency' (stale state from a failed auto-unblock
    // cascade — e.g. last_status_actor was reset to 'human' by a
    // subsequent update, so the cascade skipped it), clear the block here
    // so start_execution_run can advance kanban status to `active`.
    // Only applies to task_ ids; projects don't carry blocked_reason.
    if work_item_id.starts_with("task_") {
        let now = now_string();
        let rows_cleared = conn.execute(
            "UPDATE tasks
             SET status            = 'todo',
                 blocked_reason    = NULL,
                 last_status_actor = 'engine',
                 updated_at        = ?2
             WHERE id              = ?1
               AND deleted_at      IS NULL
               AND status          = 'blocked'
               AND (blocked_reason = 'dependency' OR blocked_reason IS NULL)",
            params![work_item_id, now],
        )?;
        if rows_cleared > 0 {
            tracing::info!(
                work_item_id = %work_item_id,
                "RequestExecution: cleared stale dependency block — all prereqs satisfied",
            );
        }
    }

    // Multi-repo Q5: route through the single resolver so the
    // explicit `bossctl work start` path refuses with the same
    // message the reconciler would have surfaced. The matching
    // sticky attention item is written by the public
    // `request_execution_with_live_check` wrapper from a separate
    // transaction — doing it here would let the bail's rollback
    // erase the kanban surface alongside the dispatch attempt.
    let resolved_repo = resolve_repo_for_work_item(conn, &work_item_id)?;
    if resolved_repo.is_none() {
        let label = repo_unresolved_kind_label(conn, &work_item_id)?;
        bail!("{}", repo_unresolved_attention_body(&work_item_id, label));
    }

    // Idempotency / re-dispatch-storm guard.
    //
    // The execution that governs whether this work item needs a *new*
    // dispatch is the one a worker may actually be attached to — i.e.
    // the most recent execution in a *live* DB status (`running` /
    // `waiting_human`). Keying off the plain "latest execution" is the
    // defect behind the R693 storm (`task_18b347260cd7da80_e`): once an
    // earlier re-dispatch stalled and was abandoned/orphaned, that
    // *terminal* row is newer than the genuinely-live run and shadows
    // it. `query_latest_execution_for_work_item` then returns the
    // terminal phantom, `execution_status_is_terminal` short-circuits
    // the guard below, and we insert yet another `ready` row that claims
    // a worker and stalls — the loop repeats every sweep.
    //
    // Preferring the live execution closes that loop: a work item with a
    // live execution is evaluated against THAT execution (and the
    // caller's runtime `is_live` oracle), not the phantom. When no live
    // execution exists we fall back to the latest row, preserving the
    // prior behaviour for `ready` / `waiting_dependency` / terminal-only
    // histories.
    let latest = query_latest_execution_for_work_item(conn, &work_item_id)?;
    let live = query_live_execution_for_work_item(conn, &work_item_id)?;
    let governing = live.clone().or_else(|| latest.clone());

    if let Some(existing) = governing {
        if !execution_status_is_terminal(&existing.status) {
            // Existing non-terminal row. Two cases:
            //   - is_live=true: a worker is genuinely attached to the
            //     slot. Keep the row, refresh priority / preferred
            //     workspace, return the same execution. (Idempotent —
            //     this is what bossctl `work start` and a kanban
            //     drag both depend on for "don't double-spawn.")
            //   - is_live=false: the row is stale (worker gone). Two sub-cases:
            //       * ci_remediation kind: re-queue it instead of abandoning.
            //         The branch/PR already exists; the human dragging to Doing
            //         (or calling `bossctl work start`) means "retry the CI fix
            //         on the existing branch," not "redo the whole chore." See
            //         the ci_failure retry design for the full invariant set.
            //       * any other kind: abandon and fall through to insert a
            //         fresh ready row, which is the normal re-dispatch path.
            //
            // Decision-point instrumentation (re-dispatch storm
            // visibility): every dispatch trigger funnels through here,
            // so a structured log at each branch makes "why did the
            // scheduler conclude this work item needs dispatch?"
            // diagnosable for ALL loops (orphan sweep, startup
            // reconcile, worker-release rescan, kanban drag) without
            // each having to instrument itself.
            let latest_id = latest.as_ref().map(|e| e.id.clone());
            let live_id = live.as_ref().map(|e| e.id.clone());
            if is_live(&existing.id) {
                tracing::info!(
                    work_item_id = %work_item_id,
                    governing_execution_id = %existing.id,
                    governing_status = %existing.status,
                    latest_execution_id = ?latest_id,
                    live_execution_id = ?live_id,
                    decision = "reuse_live",
                    "dispatch_decision: work item already has a live execution — \
                     returning it, no new dispatch",
                );
                let next_status = if existing.status == "waiting_dependency" {
                    "ready".to_owned()
                } else {
                    existing.status.clone()
                };
                let next_priority = priority.unwrap_or(existing.priority);
                let next_preferred = preferred_workspace_id
                    .clone()
                    .or(existing.preferred_workspace_id);
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
            } else if existing.kind == "ci_remediation" {
                tracing::info!(
                    work_item_id = %work_item_id,
                    governing_execution_id = %existing.id,
                    governing_status = %existing.status,
                    latest_execution_id = ?latest_id,
                    live_execution_id = ?live_id,
                    decision = "requeue_ci_remediation",
                    "dispatch_decision: governing ci_remediation execution not live — \
                     re-queuing the existing branch instead of redoing the chore",
                );
                // Stale ci_remediation — re-queue it for retry.
                //
                // For the `bossctl work start` path the task may still be
                // `status='blocked'` when request_execution is called (the
                // CLI does not flip the kanban status first, unlike the UI
                // drag). Clear the ci_failure block so start_execution_run
                // can advance the task to `active` when the worker picks up.
                // For the drag-to-Doing path the task is already `active`
                // (the UI set it before firing RequestExecution), so the
                // WHERE guard is a no-op.
                let now = now_string();
                let rows_cleared = conn.execute(
                    "UPDATE tasks
                     SET status             = 'todo',
                         blocked_reason     = NULL,
                         blocked_attempt_id = NULL,
                         last_status_actor  = 'engine',
                         updated_at         = ?2
                     WHERE id               = ?1
                       AND deleted_at       IS NULL
                       AND status           = 'blocked'
                       AND blocked_reason   IN ('ci_failure', 'ci_failure_exhausted')",
                    params![work_item_id, now],
                )?;
                if rows_cleared > 0 {
                    // Clear matching task_blocked_signals rows and insert a
                    // ci_failure_suppression so the CI watch does not
                    // immediately re-flip the task before the worker pushes
                    // a fix. Mirrors what update_work_item_as_actor does when
                    // a human drags the card out of the Blocked column.
                    conn.execute(
                        "UPDATE task_blocked_signals
                         SET cleared_at = ?2
                         WHERE work_item_id = ?1
                           AND reason IN ('ci_failure', 'ci_failure_exhausted')
                           AND cleared_at IS NULL",
                        params![work_item_id, now],
                    )?;
                    record_ci_failure_suppression_in_tx(conn, &work_item_id, &now)?;
                    tracing::info!(
                        work_item_id = %work_item_id,
                        "RequestExecution: cleared ci_failure block for bossctl retry path",
                    );
                }
                // Re-queue: move the stale cube_workspace_id into
                // preferred_workspace_id so the dispatcher can attempt to
                // re-claim the same workspace (and therefore the same
                // in-progress branch). Clearing cube_lease_id is required
                // because start_execution_run stamps fresh lease info; the
                // old lease was released by the worker on clean exit. If the
                // worker crashed, the orphan reaper eventually reconciles.
                let preferred = preferred_workspace_id
                    .clone()
                    .or_else(|| existing.cube_workspace_id.clone());
                conn.execute(
                    "UPDATE work_executions
                     SET status                 = 'ready',
                         cube_lease_id          = NULL,
                         cube_workspace_id      = NULL,
                         workspace_path         = NULL,
                         preferred_workspace_id = ?2,
                         finished_at            = NULL
                     WHERE id = ?1",
                    params![existing.id, preferred],
                )?;
                tracing::info!(
                    work_item_id = %work_item_id,
                    execution_id = %existing.id,
                    "RequestExecution: re-queued stale ci_remediation for retry",
                );
                return query_execution(conn, &existing.id)?
                    .with_context(|| format!("unknown execution: {}", existing.id));
            } else {
                tracing::info!(
                    work_item_id = %work_item_id,
                    governing_execution_id = %existing.id,
                    governing_status = %existing.status,
                    latest_execution_id = ?latest_id,
                    live_execution_id = ?live_id,
                    decision = "abandon_stale_and_redispatch",
                    "dispatch_decision: governing execution not live (worker gone) — \
                     abandoning it and creating a fresh ready execution",
                );
                let now = now_string();
                conn.execute(
                    "UPDATE work_executions
                     SET status = 'abandoned',
                         finished_at = COALESCE(finished_at, ?2)
                     WHERE id = ?1",
                    params![existing.id, now],
                )?;
            }
        } else {
            tracing::info!(
                work_item_id = %work_item_id,
                governing_execution_id = %existing.id,
                governing_status = %existing.status,
                decision = "create_fresh_after_terminal",
                "dispatch_decision: most recent execution is terminal and no live \
                 execution exists — creating a fresh ready execution",
            );
        }
    } else {
        tracing::info!(
            work_item_id = %work_item_id,
            decision = "create_fresh_no_history",
            "dispatch_decision: no prior execution — creating the first ready execution",
        );
    }

    let _ = product_id_for_work_item(conn, &work_item_id)?;

    // For revision tasks, look up the chain root's PR URL so the worker
    // knows which existing PR branch to push commits to.  Without this the
    // revision prelude in the worker prompt has no PR URL to reference and
    // the worker would have to guess — or, worse, open a new orphan PR.
    //
    // The orphan-sweep re-dispatch path is the primary caller here: when a
    // revision task was already `active` and its worker crashed, we need to
    // re-create the execution with the same `pr_url` the original dispatch
    // carried.  The chain root's `pr_url` is the authoritative source
    // because revision tasks themselves never own a `pr_url` column value.
    let revision_pr_url: Option<String> = if kind == "revision_implementation" {
        get_chain_root_task(conn, &work_item_id)?
            .and_then(|t| t.pr_url)
            .filter(|u| !u.is_empty())
    } else {
        None
    };

    insert_execution(
        conn,
        CreateExecutionInput {
            work_item_id,
            kind,
            status: Some("ready".to_owned()),
            // The early return above guarantees this is `Some(_)`;
            // we pass it through explicitly so `insert_execution`
            // doesn't redo the resolution and so per-row overrides
            // stay authoritative even when `update_task` patches
            // them between resolve and insert.
            repo_remote_url: resolved_repo,
            cube_repo_id: None,
            cube_lease_id: None,
            cube_workspace_id: None,
            workspace_path: None,
            priority,
            preferred_workspace_id,
            started_at: None,
            finished_at: None,
            prefer_is_soft: false,
            pr_url: revision_pr_url,
        },
    )
}
