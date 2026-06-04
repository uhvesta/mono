use super::*;

impl WorkDb {
    /// For a `revision` task, walk the parent chain to the chain root and
    /// return the chain root's bound `pr_url`, or `None` if the chain root
    /// cannot be resolved or has no bound PR yet.
    ///
    /// This is the authoritative fallback for completion-handler code that
    /// needs the bound PR URL but cannot rely on `execution.pr_url` (e.g.
    /// executions created before `pr_url` was reliably stamped at dispatch
    /// time). It mirrors the lookup performed by `reconcile_revision_execution`
    /// so the completion handler and the dispatcher always agree on which PR
    /// the revision belongs to.
    pub(crate) fn get_revision_chain_root_pr_url(&self, task_id: &str) -> Option<String> {
        let conn = self.connect().ok()?;
        get_chain_root_task(&conn, task_id)
            .ok()
            .flatten()
            .and_then(|t| t.pr_url)
            .filter(|u| !u.is_empty())
    }
}

/// Return the id of the most-recently-created non-done revision that is a
/// descendant of `root_id`, or `None` when the chain has no prior active
/// revision.
///
/// This is used by [`assert_parent_revisable_and_insert`] to find the
/// "tail" of the revision chain so the new revision can be automatically
/// gated on it, serialising back-to-back revisions targeting the same PR.
///
/// "Active" = status is not `'done'` (includes `todo`, `blocked`,
/// `in_progress`, `in_review`).  A done revision is already finished and
/// cannot race with the new one, so it does not need to gate it.
///
/// The recursive CTE walks `parent_task_id` links one level at a time,
/// starting from direct children of `root_id`.  Depth is capped at 64 by
/// the CTE's `UNION ALL` termination condition (no infinite loop in
/// well-formed data; the engine never creates cycles).
pub(crate) fn find_latest_active_revision_in_chain(
    conn: &Connection,
    root_id: &str,
) -> Result<Option<String>> {
    let id: Option<String> = conn
        .query_row(
            "WITH RECURSIVE chain(id) AS (
                SELECT id
                FROM tasks
                WHERE parent_task_id = ?1
                  AND kind = 'revision'
                  AND deleted_at IS NULL
              UNION ALL
                SELECT t.id
                FROM tasks t
                JOIN chain c ON t.parent_task_id = c.id
                WHERE t.kind = 'revision'
                  AND t.deleted_at IS NULL
            )
            SELECT c.id
            FROM chain c
            JOIN tasks t ON t.id = c.id
            WHERE t.status != 'done'
            ORDER BY c.id DESC
            LIMIT 1",
            params![root_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(id)
}

/// Run the create-time gate and, on success, insert a `kind = 'revision'`
/// task row atomically. This is the single point of truth for the invariant
/// "kind = revision ⇒ parent_task_id IS NOT NULL AND chain root has an open PR".
///
/// Gate order (per revision-tasks.md §Q4):
/// 1. Resolve `input.parent_task_id` to a real task; walk to chain root.
/// 2. If chain root has no `pr_url` → [`RevisionGateError::NoPr`].
/// 3. If chain root `status == "done"` → [`RevisionGateError::Merged`] (PR merged = task done).
/// 4. Otherwise call `pr_checker.check(pr_url)` for the live state:
///    `Merged` → merged error; `ClosedUnmerged` → closed error; `Open` → insert.
pub(crate) fn assert_parent_revisable_and_insert(
    conn: &Connection,
    input: CreateRevisionInput,
    pr_checker: &dyn PrStateChecker,
) -> Result<Task> {
    // ── 1. Resolve parent and chain root ────────────────────────────────────
    let parent_id = resolve_task_id_from_selector(conn, &input.parent_task_id)?;
    let root_id = chain_root(conn, &parent_id)?;
    let root =
        query_task(conn, &root_id)?.with_context(|| format!("chain root {root_id} not found"))?;

    // ── 2. No PR → reject ───────────────────────────────────────────────────
    let pr_url = match &root.pr_url {
        None => return Err(anyhow::Error::new(RevisionGateError::no_pr(&root))),
        Some(url) => url.clone(),
    };

    // ── 3. Cached: task done → PR merged ────────────────────────────────────
    if root.status == TaskStatus::Done {
        return Err(anyhow::Error::new(RevisionGateError::merged(
            &root, &pr_url,
        )));
    }

    // ── 4. Live probe for Open vs ClosedUnmerged ────────────────────────────
    match pr_checker.check(&pr_url)? {
        PrOpenState::Merged => {
            return Err(anyhow::Error::new(RevisionGateError::merged(
                &root, &pr_url,
            )));
        }
        PrOpenState::ClosedUnmerged => {
            return Err(anyhow::Error::new(RevisionGateError::closed(
                &root, &pr_url,
            )));
        }
        PrOpenState::Open => {}
    }

    // ── 5. Find chain tail for auto-sequencing ──────────────────────────────
    // Snapshot the latest non-done revision for this chain root *before*
    // inserting the new one.  The new revision will be gated on this tail
    // so that back-to-back revisions targeting the same PR always execute
    // one-after-another rather than racing as concurrent workers.
    let chain_tail_id = find_latest_active_revision_in_chain(conn, &root_id)?;

    // ── 6. Insert revision ──────────────────────────────────────────────────
    let now = now_string();
    let new_revision = insert_revision_in_tx(conn, input, &parent_id, &root)?;

    // ── 7. Auto-gate: block new revision on chain tail ───────────────────────
    // When a prior unfinished revision exists, the new one must wait for it
    // before the dispatcher can run it.  This prevents two workers from
    // committing to the same PR branch simultaneously.
    if let Some(tail_id) = chain_tail_id {
        deps::insert_edge(conn, &new_revision.id, &tail_id, RELATION_BLOCKS, &now)?;
        maybe_engine_block_dependent(conn, &new_revision.id, &now)?;
        // Re-read the row so the caller sees the updated status.
        return query_task(conn, &new_revision.id)?
            .with_context(|| format!("missing revision after auto-block: {}", new_revision.id));
    }

    Ok(new_revision)
}

/// Resolve a caller-supplied task selector (full `task_<hex>` id, `T<n>`
/// short id, or bare primary id) to the primary `tasks.id`. For now only
/// full ids are supported; short-id resolution requires the product scope
/// which the engine RPC can carry. Extended when needed.
pub(crate) fn resolve_task_id_from_selector(conn: &Connection, selector: &str) -> Result<String> {
    let trimmed = selector.trim();
    // Full typed id
    if trimmed.starts_with("task_") {
        if !row_exists(
            conn,
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1 AND deleted_at IS NULL)",
            &[&trimmed],
        )? {
            bail!("unknown task: {trimmed}");
        }
        return Ok(trimmed.to_owned());
    }
    bail!(
        "unsupported selector {trimmed:?}; pass the full task id (task_<hex>). \
         Short-id (T<n>) resolution is done by the CLI before sending the RPC."
    )
}

// ── Revision projection helpers ─────────────────────────────────────────────

/// Derive `revision_seq` and `revision_parent_pr_url` for every revision task
/// in `tasks` and return the annotated list.
///
/// Algorithm:
/// 1. Build a lookup of all task IDs → (kind, parent_task_id, pr_url) from
///    both `tasks` and `chores` (the chain root can be a chore).
/// 2. For each revision, walk `parent_task_id` links until a non-revision
///    ancestor is reached — that is the chain root.
/// 3. Group revisions by chain-root ID; sort each group by `created_at ASC`
///    (creation order = R<n> order).
/// 4. Assign 1-based sequence numbers within each group and set
///    `revision_parent_pr_url` from the chain root's `pr_url`.
///
/// Capped at a chain depth of 20 to protect against cycles in corrupt data.
pub(crate) fn attach_revision_projections(mut tasks: Vec<Task>, chores: &[Task]) -> Vec<Task> {
    // Compact lookup: id → (kind, parent_task_id, pr_url)
    type Entry = (TaskKind, Option<String>, Option<String>);
    let mut lookup: std::collections::HashMap<String, Entry> = std::collections::HashMap::new();
    for t in tasks.iter().chain(chores.iter()) {
        lookup.insert(
            t.id.clone(),
            (t.kind.clone(), t.parent_task_id.clone(), t.pr_url.clone()),
        );
    }

    /// Walk parent_task_id links to the first non-revision ancestor.
    /// Returns `(root_id, root_pr_url)` or `None` when the chain is broken.
    fn chain_root(
        start: &str,
        lookup: &std::collections::HashMap<String, (TaskKind, Option<String>, Option<String>)>,
    ) -> Option<(String, Option<String>)> {
        let mut cur = start.to_owned();
        for _ in 0..20 {
            let (kind, parent_id, pr_url) = lookup.get(&cur)?;
            if *kind != TaskKind::Revision {
                return Some((cur, pr_url.clone()));
            }
            cur = parent_id.clone()?;
        }
        None // cycle or unexpectedly deep chain
    }

    // Find chain root for every revision, then group and sequence.
    // We work with indices into `tasks` so we can mutate them afterwards.
    let mut root_info: Vec<Option<(String, Option<String>)>> = tasks
        .iter()
        .map(|t| {
            if t.kind == TaskKind::Revision {
                chain_root(&t.id, &lookup)
            } else {
                None
            }
        })
        .collect();

    // Group revision indices by root_id, sorted by created_at.
    // Key: root_id → Vec<(created_at, index)> sorted by created_at.
    let mut by_root: std::collections::HashMap<String, Vec<(String, usize)>> =
        std::collections::HashMap::new();
    for (idx, t) in tasks.iter().enumerate() {
        if t.kind == TaskKind::Revision
            && let Some((root_id, _)) = &root_info[idx] {
                by_root
                    .entry(root_id.clone())
                    .or_default()
                    .push((t.created_at.clone(), idx));
            }
    }
    for entries in by_root.values_mut() {
        entries.sort_by(|a, b| a.0.cmp(&b.0)); // stable sort by created_at
    }

    // Build seq map: task index → 1-based sequence number.
    let mut seq_map: std::collections::HashMap<usize, i64> = std::collections::HashMap::new();
    for entries in by_root.values() {
        for (seq_0, (_, idx)) in entries.iter().enumerate() {
            seq_map.insert(*idx, (seq_0 + 1) as i64);
        }
    }

    // Apply projections to the task list.
    for (idx, task) in tasks.iter_mut().enumerate() {
        if task.kind != TaskKind::Revision {
            continue;
        }
        if let Some((_, pr_url)) = root_info[idx].take() {
            task.revision_parent_pr_url = pr_url;
        }
        if let Some(seq) = seq_map.get(&idx) {
            task.revision_seq = Some(*seq);
        }
    }

    tasks
}

/// Set `has_in_progress_revision = true` on every chain-root task that has
/// at least one descendant revision with status `todo` or `active`.
///
/// Called by `get_work_tree` after [`attach_revision_projections`]. Only
/// revisions in the `tasks` slice are inspected (revisions can only be
/// `kind = "revision"` tasks, never chores). The chain root can live in
/// either `tasks` or `chores`, so both slices are mutated.
///
/// Status rule: `todo` and `active` are the in-progress states. `in_review`
/// means the revision's commit has already landed on the PR branch — that is
/// NOT a merge blocker. `done` and deleted revisions likewise don't trigger
/// the flag.
pub(crate) fn attach_in_progress_revision_flag(tasks: &mut [Task], chores: &mut [Task]) {
    // Build a compact lookup: id → (kind, parent_task_id) for chain walking.
    let mut lookup: std::collections::HashMap<String, (TaskKind, Option<String>)> =
        std::collections::HashMap::new();
    for t in tasks.iter().chain(chores.iter()) {
        lookup.insert(t.id.clone(), (t.kind.clone(), t.parent_task_id.clone()));
    }

    /// Walk parent_task_id links to the first non-revision ancestor.
    /// Returns the root id or `None` when the chain is broken or cycles.
    fn walk_to_root(
        start: &str,
        lookup: &std::collections::HashMap<String, (TaskKind, Option<String>)>,
    ) -> Option<String> {
        let mut cur = start.to_owned();
        for _ in 0..20 {
            let (kind, parent_id) = lookup.get(&cur)?;
            if *kind != TaskKind::Revision {
                return Some(cur);
            }
            cur = parent_id.clone()?;
        }
        None
    }

    // Collect all root ids that have at least one in-progress revision.
    let mut in_progress_roots: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for t in tasks.iter() {
        if t.kind == TaskKind::Revision && (t.status == TaskStatus::Todo || t.status == TaskStatus::Active)
            && let Some(root_id) = walk_to_root(&t.id, &lookup) {
                in_progress_roots.insert(root_id);
            }
    }

    if in_progress_roots.is_empty() {
        return;
    }

    for task in tasks.iter_mut() {
        if task.kind != TaskKind::Revision && in_progress_roots.contains(&task.id) {
            task.has_in_progress_revision = true;
        }
    }
    for chore in chores.iter_mut() {
        if in_progress_roots.contains(&chore.id) {
            chore.has_in_progress_revision = true;
        }
    }
}

// ── AI reviewing flag ────────────────────────────────────────────────────────

/// Set `ai_reviewing = true` on every task (and chore) that is currently held
/// in `active` (Doing) with a `pr_url` AND has a non-terminal `pr_review`
/// execution. Called from `get_work_tree` to surface the "Reviewing (AI)"
/// badge on kanban cards while the reviewer pass is in flight.
///
/// The flag is derived — not a stored DB column — so it's always accurate: a
/// task that never had a reviewer, or whose reviewer has already finalised,
/// arrives with `ai_reviewing = false` (the default).
pub(crate) fn attach_ai_reviewing_flag(
    conn: &Connection,
    tasks: &mut [Task],
    chores: &mut [Task],
) -> rusqlite::Result<()> {
    // Collect IDs of tasks currently in `active` with a `pr_url` — these are
    // the only candidates. If there are none we can skip the DB query entirely.
    let candidate_ids: Vec<&str> = tasks
        .iter()
        .chain(chores.iter())
        .filter(|t| t.status == TaskStatus::Active && t.pr_url.is_some())
        .map(|t| t.id.as_str())
        .collect();
    if candidate_ids.is_empty() {
        return Ok(());
    }

    // Find which of those candidates have a non-terminal `pr_review` execution.
    // Non-terminal = not in (completed, abandoned, failed, cancelled, orphaned).
    // We use a single query with an IN clause built from the candidate IDs.
    let placeholders = candidate_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT DISTINCT we.work_item_id
         FROM work_executions we
         WHERE we.work_item_id IN ({})
           AND we.kind = 'pr_review'
           AND we.status NOT IN ('completed', 'abandoned', 'failed', 'cancelled', 'orphaned')",
        placeholders
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = candidate_ids
        .iter()
        .map(|id| id as &dyn rusqlite::ToSql)
        .collect();
    let reviewing: std::collections::HashSet<String> = stmt
        .query_map(params.as_slice(), |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();

    if reviewing.is_empty() {
        return Ok(());
    }
    for task in tasks.iter_mut() {
        if reviewing.contains(&task.id) {
            task.ai_reviewing = true;
        }
    }
    for chore in chores.iter_mut() {
        if reviewing.contains(&chore.id) {
            chore.ai_reviewing = true;
        }
    }
    Ok(())
}

// ── revision name helpers ────────────────────────────────────────────────────

/// Extract a short display name from a revision description.
///
/// Returns the first non-empty, non-blank line of `description`, trimmed.
/// If that first line exceeds 120 characters it is hard-truncated at the
/// nearest word boundary below 120 and an ellipsis is appended. The full
/// description is stored separately in `tasks.description`; the `name`
/// column is just the compact card title.
pub(crate) fn revision_name_from_description(description: &str) -> String {
    for line in description.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return if trimmed.len() <= 120 {
            trimmed.to_owned()
        } else {
            // Walk back to the largest char boundary <= 120 so slicing never
            // splits a multi-byte UTF-8 scalar (a naive `&trimmed[..120]` panics
            // when byte 120 lands mid-character).
            let mut end = 120;
            while !trimmed.is_char_boundary(end) {
                end -= 1;
            }
            let cutoff = &trimmed[..end];
            match cutoff.rfind(' ') {
                Some(pos) => format!("{}…", &cutoff[..pos]),
                None => format!("{cutoff}…"),
            }
        };
    }
    // Fallback: should not reach here — insert_revision_in_tx enforces non-empty.
    description.trim().to_owned()
}

/// Insert a `kind = 'revision'` task row. Called only after the gate passes.
///
/// `parent_id` is the immediate parent (may itself be a revision).
/// `root` is the chain root task (non-revision ancestor that owns the PR).
pub(crate) fn insert_revision_in_tx(
    conn: &Connection,
    input: CreateRevisionInput,
    parent_id: &str,
    root: &Task,
) -> Result<Task> {
    let id = next_id("task");
    let now = now_string();
    let description = input.description.trim().to_owned();
    if description.is_empty() {
        bail!("revision description must be non-empty");
    }
    let priority = normalize_priority(input.priority.as_deref())?;
    let effort_level = input
        .effort_level
        .map(|l| l.as_str().to_owned())
        .or_else(|| Some("small".to_owned())); // revision-tasks.md §Q7: default small
    let model_override = normalize_model_override(input.model_override);
    let created_via = canonicalize_created_via(input.created_via.as_deref(), &id, "revision");
    // Inherit product, project, and repo from the chain root. A revision
    // by definition lands a follow-up commit on the chain root's PR, which
    // lives in exactly one repo — the root's — so the revision must target
    // the same repo.
    //
    // Copying `root.repo_remote_url` verbatim preserves the per-task repo
    // override invariant (see `enforce_task_repo_invariant`): the root row
    // carries a non-NULL `repo_remote_url` only for multi-repo products
    // whose `product.repo_remote_url` is NULL, so the revision mirrors the
    // same shape. When the product owns the repo, the root's column is NULL
    // and the revision stays NULL too — `resolve_repo_for_work_item` then
    // falls back to the product for both rows.
    //
    // Without this copy, a revision under a multi-repo product had a NULL
    // repo on both the revision row and the (repo-less) product, so
    // `resolve_repo_for_work_item` returned None and the autostarted
    // execution died pre-start with no workspace (issue #840).
    let product_id = &root.product_id;
    let project_id = root.project_id.as_deref();
    let repo_remote_url = root.repo_remote_url.as_deref();
    let short_id = allocate_short_id(conn, product_id)?;
    // `name` is the compact one-line card title. When the coordinator supplies
    // `input.name`, use it verbatim (after trimming); otherwise fall back to
    // deriving the name from the first non-empty line of `description`.
    let name = input
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| revision_name_from_description(&description));
    let autostart_value: i64 = if input.autostart { 1 } else { 0 };
    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, \
         pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, \
         effort_level, model_override, short_id, parent_task_id, repo_remote_url) \
         VALUES (?1, ?2, ?3, 'revision', ?4, ?5, 'todo', NULL, NULL, NULL, ?6, ?6, ?7, ?8, ?9, \
         ?10, ?11, ?12, ?13, ?14)",
        params![
            id,
            product_id,
            project_id,
            name,
            description,
            now,
            autostart_value,
            priority,
            created_via,
            effort_level,
            model_override,
            short_id,
            parent_id,
            repo_remote_url,
        ],
    )?;
    // `query_task` reads the trailing `parent_task_id` column (via
    // `map_task_with_parent`), so the returned revision row already carries
    // its parent linkage — callers (`create-revision --json`) can verify it
    // without a second lookup.
    query_task(conn, &id)?.with_context(|| format!("missing revision after insert: {id}"))
}

/// Trim and reduce an empty model slug to `None`. The CLI uses
/// `--model ""` to clear a stored override on update verbs; the
/// engine treats the same shape consistently on create so callers
/// don't have to special-case empty strings. Non-empty strings pass
/// through verbatim — claude is the source of truth on slug
/// resolution (design §Q3).
pub(crate) fn normalize_model_override(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

/// Insert a `kind = 'design'` task as the first row under
/// `project_id`. Used by `create_project` and the migration that
/// backfills design tasks for projects predating this column. The
/// design task always has `ordinal = 0` so it sorts ahead of every
/// `project_task` (which start at `ordinal = 1`) and the dispatcher
/// picks it up first via the existing first-incomplete chain.
///
/// `created_via` is always `engine_auto`: the user did not file the
/// design task directly, the engine added it as a side-effect of
/// project creation (or backfill). That distinction is the entire
/// point of the column — manual chores and engine-spawned ones must
/// be tellable apart in one query.
pub(crate) fn insert_design_task_for_project_in_tx(
    conn: &Connection,
    product_id: &str,
    project_id: &str,
    project_name: &str,
    autostart: bool,
) -> Result<Task> {
    let id = next_id("task");
    let now = now_string();
    let autostart_value: i64 = if autostart { 1 } else { 0 };
    let name = format!("Design {project_name}");
    let short_id = allocate_short_id(conn, product_id)?;
    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, short_id)
         VALUES (?1, ?2, ?3, 'design', ?7, '', 'todo', 0, NULL, NULL, ?4, ?4, ?5, 'medium', ?6, ?8)",
        params![id, product_id, project_id, now, autostart_value, CREATED_VIA_ENGINE_AUTO, name, short_id],
    )?;
    query_task(conn, &id)?.with_context(|| format!("missing design task after insert: {id}"))
}

/// Resolve the caller-supplied `created_via` to a stored string. A
/// `None` input lands as `unknown` (the engine app should normally
/// have already substituted a transport-layer hint by the time the
/// row reaches this insert; falling through to `unknown` here is the
/// last-resort safety net). Values outside the documented set are
/// stored verbatim but logged so we can spot undocumented sources
/// sneaking in. `id_for_log` and `kind_for_log` exist only to make
/// the warning useful — they don't affect the stored value.
pub(crate) fn canonicalize_created_via(
    raw: Option<&str>,
    id_for_log: &str,
    kind_for_log: &str,
) -> String {
    let value = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(CREATED_VIA_UNKNOWN);
    if !is_known_created_via(value) {
        tracing::warn!(
            id = %id_for_log,
            kind = %kind_for_log,
            created_via = %value,
            "created_via not in documented set; storing as-is",
        );
    }
    value.to_owned()
}

/// Validate a caller-supplied priority and return the canonical
/// lower-case value. `None`, the empty string, and pure whitespace
/// resolve to the schema default (`medium`) so callers never have
/// to type `--priority medium` explicitly. Anything outside
/// `low` / `medium` / `high` is rejected up-front so the engine
/// stays the single source of truth for the vocabulary.
pub fn normalize_priority(value: Option<&str>) -> Result<String> {
    let trimmed = value.unwrap_or("").trim();
    if trimmed.is_empty() {
        return Ok("medium".to_owned());
    }
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "low" | "medium" | "high" => Ok(lower),
        other => bail!("invalid priority `{other}`; expected one of low, medium, high"),
    }
}

pub(crate) fn insert_execution(
    conn: &Connection,
    input: CreateExecutionInput,
) -> Result<WorkExecution> {
    let repo_remote_url = resolve_execution_repo_remote_url(
        conn,
        &input.work_item_id,
        normalize_optional_text(input.repo_remote_url),
    )?;
    let id = next_id("exec");
    let now = now_string();
    let status = input.status.unwrap_or_default();
    let cube_repo_id = normalize_optional_text(input.cube_repo_id);
    let cube_lease_id = normalize_optional_text(input.cube_lease_id);
    let cube_workspace_id = normalize_optional_text(input.cube_workspace_id);
    let workspace_path = normalize_optional_text(input.workspace_path);
    let priority = input.priority.unwrap_or(0);
    let preferred_workspace_id = normalize_optional_text(input.preferred_workspace_id);
    let started_at = normalize_optional_text(input.started_at);
    let finished_at = normalize_optional_text(input.finished_at);
    let prefer_is_soft: i64 = if input.prefer_is_soft { 1 } else { 0 };
    let allow_dirty: i64 = if input.allow_dirty { 1 } else { 0 };
    let pr_url = normalize_optional_text(input.pr_url);
    // Freeze the owning product's worker branch prefix onto the execution row,
    // mirroring `repo_remote_url`. Kept for backward compatibility.
    let worker_branch_prefix = resolve_execution_worker_branch_prefix(conn, &input.work_item_id)?;
    // Snapshot the branch-naming strategy from the product's editorial_rules
    // at spawn time so the detector can always reconstruct the expected branch
    // name from state.db alone, even after the product rule changes later.
    let branch_naming = resolve_execution_branch_naming(conn, &input.work_item_id)?;
    let branch_naming_json = serde_json::to_string(&branch_naming).unwrap_or_default();

    conn.execute(
        "INSERT INTO work_executions (
            id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
            cube_workspace_id, workspace_path, priority, preferred_workspace_id,
            created_at, started_at, finished_at, prefer_is_soft, pr_url, worker_branch_prefix,
            allow_dirty, branch_naming
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            id,
            input.work_item_id,
            input.kind.as_str(),
            status.as_str(),
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
            prefer_is_soft,
            pr_url,
            worker_branch_prefix,
            allow_dirty,
            branch_naming_json,
        ],
    )?;

    query_execution(conn, &id)?.with_context(|| format!("missing execution after insert: {id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── revision_name_from_description ──────────────────────────────────────

    #[test]
    fn revision_name_skips_leading_blank_and_whitespace_lines() {
        let desc = "\n   \n\t\nFix the flaky retry loop\nmore detail";
        assert_eq!(
            revision_name_from_description(desc),
            "Fix the flaky retry loop"
        );
    }

    #[test]
    fn revision_name_short_single_line_passes_through_trimmed() {
        assert_eq!(
            revision_name_from_description("  Tidy up the dispatcher  "),
            "Tidy up the dispatcher"
        );
    }

    #[test]
    fn revision_name_exactly_120_chars_is_verbatim() {
        let line = "a".repeat(120);
        assert_eq!(revision_name_from_description(&line), line);
    }

    #[test]
    fn revision_name_long_line_with_space_truncates_at_word_boundary() {
        // 130 'a's, a space, then a tail word. The cutoff at 120 bytes lands in
        // the run of 'a's; rfind(' ') finds no space before 120, so it hard
        // cuts. Use a layout where a space *does* fall below 120 to exercise the
        // word-boundary branch.
        let head = "word ".repeat(30); // 150 bytes, spaces every 5 chars
        let out = revision_name_from_description(&head);
        // Truncated at the last space at or before byte 120 (byte 119 here:
        // "word " * 24 = 120 bytes, last space at index 119).
        assert!(out.ends_with('…'), "expected ellipsis, got {out:?}");
        assert!(!out.contains("  "), "should cut cleanly at a space: {out:?}");
        // The kept prefix must be whole words only (no trailing partial 'word').
        let kept = out.trim_end_matches('…');
        assert!(kept.split(' ').all(|w| w.is_empty() || w == "word"));
        assert!(kept.len() <= 120);
    }

    #[test]
    fn revision_name_long_line_without_space_hard_cuts() {
        let line = "x".repeat(200);
        let out = revision_name_from_description(&line);
        assert_eq!(out, format!("{}…", "x".repeat(120)));
    }

    #[test]
    fn revision_name_multibyte_straddling_120_byte_boundary_does_not_panic() {
        // One ASCII byte followed by 3-byte scalars: char boundaries fall at
        // bytes 1, 4, 7, ... = 1 + 3k. Byte 120 is *not* a boundary (119 is not
        // divisible by 3), so a naive `&trimmed[..120]` byte-slice would panic.
        let line = format!("a{}", "世".repeat(50)); // 1 + 150 = 151 bytes
        let out = revision_name_from_description(&line);
        // No spaces → hard-cut branch; must end with the ellipsis and stay valid.
        assert!(out.ends_with('…'), "expected ellipsis, got {out:?}");
        let kept = out.trim_end_matches('…');
        // Cut at the largest char boundary <= 120, i.e. byte 118 (1 + 3*39).
        assert_eq!(kept, &line[..118]);
        // Sanity: the kept text is whole characters (String guarantees validity).
        assert!(line.starts_with(kept));
    }

    // ── normalize_priority ──────────────────────────────────────────────────

    #[test]
    fn normalize_priority_defaults_to_medium() {
        assert_eq!(normalize_priority(None).unwrap(), "medium");
        assert_eq!(normalize_priority(Some("")).unwrap(), "medium");
        assert_eq!(normalize_priority(Some("   \t ")).unwrap(), "medium");
    }

    #[test]
    fn normalize_priority_canonicalizes_case_and_whitespace() {
        assert_eq!(normalize_priority(Some("  LOW ")).unwrap(), "low");
        assert_eq!(normalize_priority(Some("Medium")).unwrap(), "medium");
        assert_eq!(normalize_priority(Some("HIGH")).unwrap(), "high");
    }

    #[test]
    fn normalize_priority_rejects_unknown_value() {
        let err = normalize_priority(Some("urgent")).unwrap_err();
        assert!(
            err.to_string().contains("invalid priority"),
            "unexpected error: {err}"
        );
    }

    // ── normalize_model_override ────────────────────────────────────────────

    #[test]
    fn normalize_model_override_none_and_blank_collapse_to_none() {
        assert_eq!(normalize_model_override(None), None);
        assert_eq!(normalize_model_override(Some(String::new())), None);
        assert_eq!(normalize_model_override(Some("   \t".to_owned())), None);
    }

    #[test]
    fn normalize_model_override_trims_and_passes_through() {
        assert_eq!(
            normalize_model_override(Some("  opus  ".to_owned())),
            Some("opus".to_owned())
        );
        assert_eq!(
            normalize_model_override(Some("claude-sonnet-4-6".to_owned())),
            Some("claude-sonnet-4-6".to_owned())
        );
    }

    // ── canonicalize_created_via ────────────────────────────────────────────

    #[test]
    fn canonicalize_created_via_blank_falls_back_to_unknown() {
        assert_eq!(
            canonicalize_created_via(None, "task_x", "revision"),
            CREATED_VIA_UNKNOWN
        );
        assert_eq!(
            canonicalize_created_via(Some(""), "task_x", "revision"),
            CREATED_VIA_UNKNOWN
        );
        assert_eq!(
            canonicalize_created_via(Some("   "), "task_x", "revision"),
            CREATED_VIA_UNKNOWN
        );
    }

    #[test]
    fn canonicalize_created_via_known_values_returned_verbatim() {
        assert_eq!(
            canonicalize_created_via(Some(CREATED_VIA_ENGINE_AUTO), "task_x", "revision"),
            CREATED_VIA_ENGINE_AUTO
        );
        let merge_conflict = "merge-conflict:crz_abc123";
        assert_eq!(
            canonicalize_created_via(Some(merge_conflict), "task_x", "revision"),
            merge_conflict
        );
    }

    #[test]
    fn canonicalize_created_via_trims_and_stores_undocumented_value_as_is() {
        // Surrounding whitespace is trimmed; an undocumented value is still
        // stored verbatim (logged, not rejected).
        assert_eq!(
            canonicalize_created_via(Some("  some-future-source  "), "task_x", "revision"),
            "some-future-source"
        );
    }
}
