use super::*;

pub(crate) fn slugify(name: &str) -> String {
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

pub(crate) enum ItemKind {
    Product,
    Project,
    Task,
}

/// If `id` looks like a friendly work-item selector (`T42`, `t42`, `P7`,
/// `p7`), query the DB by short_id and return the matching primary id.
/// Returns `Ok(None)` when `id` is not a friendly-id form or when no row
/// matches; callers should treat the original id as-is in that case.
pub(crate) fn resolve_friendly_work_item_id(conn: &Connection, id: &str) -> Result<Option<String>> {
    resolve_friendly_work_item_id_inner(conn, id, false)
}

/// Variant of [`resolve_friendly_work_item_id`] that, when
/// `include_deleted` is true, resolves a `T<n>` short id even if its
/// task row carries a `deleted_at` tombstone. Only `restore` needs
/// this — every other resolution path wants the live-only view and
/// calls through the plain wrapper above.
pub(crate) fn resolve_friendly_work_item_id_inner(
    conn: &Connection,
    id: &str,
    include_deleted: bool,
) -> Result<Option<String>> {
    if id.len() < 2 {
        return Ok(None);
    }
    let first = id.as_bytes()[0];
    if first != b'T' && first != b't' && first != b'P' && first != b'p' {
        return Ok(None);
    }
    let n: i64 = match id[1..].parse() {
        Ok(n) if n > 0 => n,
        _ => return Ok(None),
    };
    let task_sql = if include_deleted {
        "SELECT id FROM tasks WHERE short_id = ?1 LIMIT 1"
    } else {
        "SELECT id FROM tasks WHERE short_id = ?1 AND deleted_at IS NULL LIMIT 1"
    };
    if let Some(primary_id) = conn
        .query_row(task_sql, params![n], |row| row.get::<_, String>(0))
        .optional()?
    {
        return Ok(Some(primary_id));
    }
    if let Some(primary_id) = conn
        .query_row(
            "SELECT id FROM projects WHERE short_id = ?1 LIMIT 1",
            params![n],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(Some(primary_id));
    }
    Ok(None)
}

pub(crate) fn classify_id(id: &str) -> Result<ItemKind> {
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

/// Resolve a single edge endpoint into its [`DependencyEdge`] view.
/// `peer_id` is the *other* end of the edge (the prerequisite when
/// the edge sits in the prerequisites list, the dependent when it
/// sits in the dependents list). Looks up the row's status / name /
/// kind so the view is fully self-contained. A peer that no longer
/// resolves (soft-deleted task; concurrent delete) renders as
/// `kind = "unknown"` with empty name and `status = "missing"` —
/// the human renderer surfaces it instead of dropping the row, so
/// the user can spot dangling edges and clean them up.
pub(crate) fn resolve_dependency_edge(
    conn: &Connection,
    peer_id: &str,
    relation: &str,
) -> Result<DependencyEdge> {
    if peer_id.starts_with("proj_") {
        if let Some(project) = query_project(conn, peer_id)? {
            return Ok(DependencyEdge {
                id: project.id,
                relation: relation.to_owned(),
                kind: "project".to_owned(),
                name: project.name,
                status: project.status,
            });
        }
    } else if peer_id.starts_with("task_")
        && let Some(task) = query_task(conn, peer_id)? {
            let kind = match task.kind {
                TaskKind::Chore => "chore",
                _ => "task",
            };
            return Ok(DependencyEdge {
                id: task.id,
                relation: relation.to_owned(),
                kind: kind.to_owned(),
                name: task.name,
                status: task.status.to_string(),
            });
        }
    Ok(DependencyEdge {
        id: peer_id.to_owned(),
        relation: relation.to_owned(),
        kind: "unknown".to_owned(),
        name: String::new(),
        status: "missing".to_owned(),
    })
}

/// Mutate `items` in place to retain only the rows that match
/// `filter`. The closure pair lets the same helper drive task,
/// chore, and project lists — they all key on `id` and `status`,
/// just on different row types.
///
/// `Unblocked` and `BlockedByDeps` need the full set of gated ids
/// for the open product, computed once via a pair of joins (see
/// [`compute_gated_work_item_ids`]). `PrerequisitesOf` and
/// `DependentsOf` need only the edge listing for the named row, so
/// they walk the existing dep helpers directly.
pub(crate) fn apply_dep_filter<T, F, G>(
    conn: &Connection,
    filter: &DependencyFilter,
    id_of: F,
    status_of: G,
    items: &mut Vec<T>,
) -> Result<()>
where
    F: Fn(&T) -> &str,
    G: Fn(&T) -> &str,
{
    match filter {
        DependencyFilter::PrerequisitesOf { id } => {
            let edges = deps::prerequisites_of(conn, id, None)?;
            let allowed: HashSet<String> =
                edges.into_iter().map(|edge| edge.prerequisite_id).collect();
            items.retain(|item| allowed.contains(id_of(item)));
        }
        DependencyFilter::DependentsOf { id } => {
            let edges = deps::dependents_of(conn, id, None)?;
            let allowed: HashSet<String> =
                edges.into_iter().map(|edge| edge.dependent_id).collect();
            items.retain(|item| allowed.contains(id_of(item)));
        }
        DependencyFilter::Unblocked => {
            let gated = compute_gated_work_item_ids(conn)?;
            items.retain(|item| status_of(item) == "todo" && !gated.contains(id_of(item)));
        }
        DependencyFilter::BlockedByDeps => {
            let gated = compute_gated_work_item_ids(conn)?;
            items.retain(|item| gated.contains(id_of(item)));
        }
    }
    Ok(())
}

/// Set of work item ids that have at least one `blocks` edge to a
/// prerequisite that has not reached a satisfied status. Tasks /
/// chores satisfy on `status = 'done'`; projects also satisfy on
/// `archived` (Q4 / Q10). Computed via two SQL joins so the helper
/// does one round-trip regardless of the dependent count.
pub(crate) fn compute_gated_work_item_ids(conn: &Connection) -> Result<HashSet<String>> {
    let mut ids: HashSet<String> = HashSet::new();
    let mut stmt = conn.prepare(
        "SELECT DISTINCT d.dependent_id
         FROM work_item_dependencies d
         JOIN tasks t ON t.id = d.prerequisite_id
         WHERE d.relation = 'blocks'
           AND t.deleted_at IS NULL
           AND t.status != 'done'",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        ids.insert(row?);
    }
    let mut stmt = conn.prepare(
        "SELECT DISTINCT d.dependent_id
         FROM work_item_dependencies d
         JOIN projects p ON p.id = d.prerequisite_id
         WHERE d.relation = 'blocks'
           AND p.status NOT IN ('done', 'archived')",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        ids.insert(row?);
    }
    Ok(ids)
}

/// Stamp a dependent's status to `blocked` and `last_status_actor`
/// to `'engine'` if (a) the dependent is currently in a status
/// other than `blocked`, `done`, `archived`, and (b) it has at least
/// one unmet gating prereq. No-op otherwise.
///
/// Used at edge-creation time (`add_dependency`): a brand-new edge
/// that introduces a gating prereq must move its dependent to
/// `blocked` so the kanban and dispatcher reflect the new gate.
/// The reverse (cascade-on-prereq-regression) deliberately does NOT
/// call this — see the comment on
/// [`cascade_dependents_after_prereq_status_change`].
pub(crate) fn maybe_engine_block_dependent(
    conn: &Connection,
    dependent_id: &str,
    now_epoch: &str,
) -> Result<()> {
    let gating = deps::gating_prereqs_for(conn, dependent_id)?;
    if gating.is_empty() {
        return Ok(());
    }
    let current_status = deps::lookup_work_item_status(conn, dependent_id)?;
    let Some(current) = current_status else {
        return Ok(());
    };
    if matches!(current.as_str(), "blocked" | "done" | "archived") {
        return Ok(());
    }
    write_engine_status(conn, dependent_id, "blocked", now_epoch)?;
    // Stamp blocked_reason so the user-override path in
    // request_execution_in_tx_with_live_check can identify and clear
    // stale dependency blocks consistently (the backfill migration
    // covered pre-existing rows; this covers new auto-blocks).
    if dependent_id.starts_with("task_") {
        conn.execute(
            "UPDATE tasks SET blocked_reason = 'dependency'
             WHERE id = ?1 AND status = 'blocked' AND deleted_at IS NULL",
            [dependent_id],
        )?;
    }
    Ok(())
}

/// Flip a dependent off `blocked` if (a) its current status is
/// `blocked`, (b) the block was engine-owned — either
/// `blocked_reason = 'dependency'` (the authoritative signal set by
/// [`maybe_engine_block_dependent`]) or, for items that pre-date that
/// column, `blocked_reason IS NULL AND last_status_actor = 'engine'`
/// — and (c) no gating prereqs remain. Items blocked for other reasons
/// (merge_conflict, ci_failure) or manually by a human are left alone.
///
/// Returns `true` when an unblock was written, `false` when the item
/// was skipped (not blocked, not engine-owned, or still gated). This
/// lets callers (and the periodic dep-unblock sweep) distinguish a
/// real action from a no-op without scanning the DB a second time.
///
/// Emits a `tracing::info!` line on each successful unblock so the
/// chain `prereq → done → dependent unblocked` is visible after the
/// fact in the engine log — without it, an auto-unblock that races
/// past a sleeping observer is invisible and the next bug report
/// degenerates into "did the cascade fire or not?".
pub(crate) fn maybe_engine_unblock_dependent(
    conn: &Connection,
    dependent_id: &str,
    now_epoch: &str,
) -> Result<bool> {
    let current = match deps::lookup_work_item_status(conn, dependent_id)? {
        Some(s) => s,
        None => return Ok(false),
    };
    if current != "blocked" {
        return Ok(false);
    }
    // Guard: only auto-unblock if the engine was responsible for the block.
    // For tasks, `blocked_reason = 'dependency'` is the canonical signal —
    // it is set atomically by `maybe_engine_block_dependent` and never set
    // by any human-facing update path.  Accept `blocked_reason IS NULL AND
    // last_status_actor = 'engine'` as a fallback for rows that were
    // auto-blocked before the blocked_reason column existed.
    // For projects (no blocked_reason column), fall back to the actor check.
    let actor = lookup_last_status_actor(conn, dependent_id)?;
    let eligible = if dependent_id.starts_with("task_") {
        match lookup_blocked_reason(conn, dependent_id)?.as_deref() {
            Some("dependency") => true,
            None => actor.as_deref() == Some("engine"),
            _ => false, // merge_conflict, ci_failure, etc. — different cascade owners
        }
    } else {
        actor.as_deref() == Some("engine")
    };
    if !eligible {
        return Ok(false);
    }
    let gating = deps::gating_prereqs_for(conn, dependent_id)?;
    if !gating.is_empty() {
        return Ok(false);
    }
    write_engine_status(conn, dependent_id, "todo", now_epoch)?;
    // Clear blocked_reason so it doesn't linger on a todo row.
    if dependent_id.starts_with("task_") {
        conn.execute(
            "UPDATE tasks SET blocked_reason = NULL
             WHERE id = ?1 AND deleted_at IS NULL",
            [dependent_id],
        )?;
    }
    tracing::info!(
        dependent_id,
        "engine: auto-unblocked dependent — all gating prereqs satisfied",
    );
    // Atomically create or promote the execution to `ready` so the
    // coordinator can dispatch this task on the next kick. Without
    // this, the `waiting_dependency` execution that was created when
    // the chore was first blocked would never be promoted to `ready`
    // unless an external event (frontend request, reconciler kick)
    // happened to trigger `reconcile_product_executions`. Only applies
    // to task_ ids; projects don't have `work_executions` rows.
    if dependent_id.starts_with("task_") {
        let kind = execution_kind_for_work_item(conn, dependent_id)?;
        let mut reconcile_result = ExecutionReconcileResult::default();
        reconcile_work_item_execution(conn, &mut reconcile_result, dependent_id, kind, ExecutionStatus::Ready)?;
    }
    Ok(true)
}

/// Walk every `blocks` dependent of `prereq_id` and run the
/// auto-unblock check when the prereq has just reached a satisfied
/// status. Non-satisfying transitions (e.g. a prereq dragged from
/// `done` back to `backlog`) intentionally do *not* re-block the
/// dependent: a row that has already been unblocked may be running
/// or in `in_review`, and yanking it back to `blocked` from under
/// a worker would lose state. The dispatcher's `gating_prereqs_for`
/// check is the safety net — a regressed prereq immediately re-gates
/// any future dispatch of its dependents — so the cascade can stay
/// purely additive.
pub(crate) fn cascade_dependents_after_prereq_status_change(
    conn: &Connection,
    prereq_id: &str,
    new_prereq_status: &str,
    now_epoch: &str,
) -> Result<()> {
    // Fire the cascade when the prereq reaches any status that *might*
    // satisfy at least one class of dependent:
    //   - `done` / `archived` satisfy all dependents (standard rule).
    //   - `in_review` satisfies revision dependents specifically
    //     (the PR is open; the revision can push to it).
    //
    // `maybe_engine_unblock_dependent` re-evaluates each dependent's
    // full gating list via `gating_prereqs_for`, which is revision-
    // aware, so non-revision dependents are not inadvertently unblocked
    // by an `in_review` transition.
    let might_satisfy =
        deps::status_satisfies(prereq_id, new_prereq_status) || new_prereq_status == "in_review";
    if !might_satisfy {
        return Ok(());
    }
    let dependents = deps::dependents_of(conn, prereq_id, Some("blocks"))?;
    for edge in dependents {
        maybe_engine_unblock_dependent(conn, &edge.dependent_id, now_epoch)?;
    }
    Ok(())
}

/// Internal write that stamps `last_status_actor = 'engine'` on the
/// row. Used by the auto-block / unblock paths. Returns the new
/// status.
pub(crate) fn write_engine_status(
    conn: &Connection,
    work_item_id: &str,
    new_status: &str,
    now_epoch: &str,
) -> Result<()> {
    if work_item_id.starts_with("proj_") {
        conn.execute(
            "UPDATE projects
             SET status = ?2, last_status_actor = 'engine', updated_at = ?3
             WHERE id = ?1",
            params![work_item_id, new_status, now_epoch],
        )?;
    } else if work_item_id.starts_with("task_") {
        conn.execute(
            "UPDATE tasks
             SET status = ?2, last_status_actor = 'engine', updated_at = ?3
             WHERE id = ?1 AND deleted_at IS NULL",
            params![work_item_id, new_status, now_epoch],
        )?;
    }
    Ok(())
}

/// Q4 case 1: refuse a manual move from `blocked` to anything else
/// while the row still has at least one unmet `blocks` prereq. The
/// alternative — letting the user override and run anyway —
/// recreates the original ambiguous "blocked" flag, which the design
/// explicitly rejects.
///
/// Manual moves *into* `blocked`, and any move when no edges gate
/// the row, are allowed.
pub(crate) fn refuse_manual_move_off_blocked_while_gated(
    conn: &Connection,
    work_item_id: &str,
    previous_status: &str,
    new_status: &str,
) -> Result<()> {
    if previous_status != "blocked" || new_status == "blocked" {
        return Ok(());
    }
    let gating = deps::gating_prereqs_for(conn, work_item_id)?;
    if gating.is_empty() {
        return Ok(());
    }
    let names = gating.join(", ");
    bail!(
        "cannot move {work_item_id} to {new_status}: gated by [{names}] (use `boss <kind> depend rm` to remove)"
    );
}

pub(crate) fn lookup_blocked_reason(
    conn: &Connection,
    work_item_id: &str,
) -> Result<Option<String>> {
    if work_item_id.starts_with("task_") {
        return conn
            .query_row(
                "SELECT blocked_reason FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
                params![work_item_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(Into::into)
            .map(|opt| opt.flatten());
    }
    Ok(None)
}

pub(crate) fn lookup_last_status_actor(
    conn: &Connection,
    work_item_id: &str,
) -> Result<Option<String>> {
    if work_item_id.starts_with("proj_") {
        return conn
            .query_row(
                "SELECT last_status_actor FROM projects WHERE id = ?1",
                params![work_item_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into);
    }
    if work_item_id.starts_with("task_") {
        return conn
            .query_row(
                "SELECT last_status_actor FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
                params![work_item_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into);
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── slugify ─────────────────────────────────────────────────────────────

    #[test]
    fn slugify_lowercases_and_collapses_separators() {
        assert_eq!(slugify("  Hello,  World!! "), "hello-world");
    }

    #[test]
    fn slugify_simple_word_passes_through_lowercased() {
        assert_eq!(slugify("Dispatcher"), "dispatcher");
    }

    #[test]
    fn slugify_collapses_runs_of_non_alphanumerics_to_single_dash() {
        assert_eq!(slugify("a___b---c   d"), "a-b-c-d");
    }

    #[test]
    fn slugify_trims_leading_and_trailing_dashes() {
        assert_eq!(slugify("--foo--"), "foo");
        assert_eq!(slugify("!!!bar???"), "bar");
    }

    #[test]
    fn slugify_keeps_internal_digits() {
        assert_eq!(slugify("Boss Engine v2"), "boss-engine-v2");
    }

    #[test]
    fn slugify_all_punctuation_yields_empty_string() {
        assert_eq!(slugify("!!! ??? ..."), "");
    }

    // ── classify_id ─────────────────────────────────────────────────────────

    #[test]
    fn classify_id_recognises_each_prefix() {
        assert!(matches!(
            classify_id("prod_abc").unwrap(),
            ItemKind::Product
        ));
        assert!(matches!(
            classify_id("proj_abc").unwrap(),
            ItemKind::Project
        ));
        assert!(matches!(classify_id("task_abc").unwrap(), ItemKind::Task));
    }

    #[test]
    fn classify_id_rejects_unknown_prefix() {
        // `ItemKind` has no `Debug`, so match rather than `unwrap_err()`.
        match classify_id("exec_abc") {
            Ok(_) => panic!("expected an error for an unknown prefix"),
            Err(err) => assert!(
                err.to_string().contains("unknown work item id format"),
                "unexpected error: {err}"
            ),
        }
        assert!(classify_id("").is_err());
    }
}
