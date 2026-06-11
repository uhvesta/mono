use super::*;

/// Walk `tasks.parent_task_id` from `task_id` to find the originating
/// non-revision task (the "chain root") — the task that owns the PR.
///
/// Revision tasks form chains: a revision of a revision is allowed (OQ2),
/// and all revisions in a chain share the chain root's PR. This helper
/// returns the ID of the first ancestor whose `kind` is not `'revision'`.
///
/// **Broken-parent handling**: if a row's `parent_task_id` points to a
/// task that no longer exists (soft-deleted or missing), walking stops at
/// the deepest reachable ancestor rather than returning an error. The
/// caller receives the last successfully-resolved ID, which is the closest
/// meaningful root we have. This matches the design doc (R8 mitigation).
///
/// **Cycle guard**: the walk is bounded by `MAX_CHAIN_DEPTH` to prevent an
/// infinite loop if the data is corrupt. Hitting the cap is treated as a
/// broken-parent condition — the deepest reached ID is returned.
pub(crate) fn chain_root(conn: &Connection, task_id: &str) -> Result<String> {
    const MAX_CHAIN_DEPTH: usize = 64;
    // `last_resolved` tracks the most recent ID that was successfully found
    // in the DB. We advance it only after a successful lookup so that if the
    // next candidate is missing we can return the last good one.
    let mut last_resolved = task_id.to_owned();
    let mut next = Some(task_id.to_owned());
    for _ in 0..MAX_CHAIN_DEPTH {
        let candidate = match next.take() {
            Some(id) => id,
            None => break,
        };
        let row: Option<(String, Option<String>)> = conn
            .query_row(
                "SELECT kind, parent_task_id FROM tasks WHERE id = ?1",
                params![candidate],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match row {
            None => break, // candidate not found; return last_resolved
            Some((kind, parent_id)) => {
                last_resolved = candidate;
                if kind != "revision" || parent_id.is_none() {
                    break; // reached a non-revision or a revision with no parent
                }
                next = parent_id;
            }
        }
    }
    Ok(last_resolved)
}

/// Flip every `in_review` revision whose chain root is `chain_root_id`
/// to `done`.  Called from `mark_chore_pr_merged` so that when the
/// parent PR merges all in-review revisions finish in the same
/// transaction (per OQ7: done == parent PR merged or closed).
///
/// The revision's own `pr_url` is intentionally left `NULL` — the chain
/// root's `pr_url` is the source of truth for the PR that delivered the
/// revision's commit.
pub(crate) fn flip_in_review_revisions_to_done(conn: &Connection, chain_root_id: &str, now: &str) -> Result<()> {
    // Find all revisions whose immediate `parent_task_id` chain leads to
    // `chain_root_id`.  Because chains are short (typically 1-3 deep) and
    // bounded by `MAX_CHAIN_DEPTH = 64`, we collect all revision IDs
    // belonging to this chain root and bulk-update them.
    let revision_ids = collect_chain_revision_ids(conn, chain_root_id)?;
    if revision_ids.is_empty() {
        return Ok(());
    }
    for rev_id in &revision_ids {
        conn.execute(
            "UPDATE tasks
             SET status            = 'done',
                 updated_at        = ?2,
                 last_status_actor = 'engine',
                 blocked_reason    = NULL,
                 blocked_attempt_id = NULL
             WHERE id = ?1
               AND kind = 'revision'
               AND status = 'in_review'
               AND deleted_at IS NULL",
            params![rev_id, now],
        )?;
    }
    Ok(())
}

/// Collect the ids of every revision task in the chain rooted at
/// `chain_root_id`, using BFS over `parent_task_id`.
pub(crate) fn collect_chain_revision_ids(conn: &Connection, chain_root_id: &str) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    let mut frontier = vec![chain_root_id.to_owned()];
    for _ in 0..64 {
        if frontier.is_empty() {
            break;
        }
        let mut next_frontier = Vec::new();
        for parent_id in &frontier {
            let mut stmt = conn.prepare_cached(
                "SELECT id FROM tasks WHERE parent_task_id = ?1 AND kind = 'revision' AND deleted_at IS NULL",
            )?;
            let children: Vec<String> = stmt
                .query_map([parent_id], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect();
            for child in children {
                ids.push(child.clone());
                next_frontier.push(child);
            }
        }
        frontier = next_frontier;
    }
    Ok(ids)
}

/// Block every revision in the chain whose status is still
/// pre-dispatch (i.e. the worker never finished pushing a commit) when
/// the parent PR merges or closes.  These revisions can no longer
/// deliver to the parent PR's branch (it's gone), so they must not be
/// dispatched.
///
/// Revisions already in `in_review` are handled by
/// [`flip_in_review_revisions_to_done`] (their commit rode the PR to
/// completion).  Revisions already `done`, `archived`, or
/// `blocked: parent_pr_closed` are already in a terminal or equivalent
/// state and are skipped.
///
/// Each newly blocked revision gets a `work_attention_items` row so
/// the kanban card explains what happened and points the operator to
/// `boss task create` for follow-up work.
pub(crate) fn block_pending_revisions_on_parent_close(conn: &Connection, chain_root_id: &str, now: &str) -> Result<()> {
    let revision_ids = collect_chain_revision_ids(conn, chain_root_id)?;
    if revision_ids.is_empty() {
        return Ok(());
    }
    for rev_id in &revision_ids {
        let rows_changed = conn.execute(
            "UPDATE tasks
             SET status            = 'blocked',
                 blocked_reason    = 'parent_pr_closed',
                 last_status_actor = 'engine',
                 updated_at        = ?2
             WHERE id = ?1
               AND kind = 'revision'
               AND status NOT IN ('blocked', 'done', 'archived', 'in_review')
               AND deleted_at IS NULL",
            params![rev_id, now],
        )?;
        if rows_changed > 0 {
            tracing::info!(
                revision_id = %rev_id,
                chain_root_id,
                "block_pending_revisions: parent PR closed/merged; blocking revision",
            );
            let attn_id = next_id("attn");
            conn.execute(
                "INSERT INTO work_attention_items
                     (id, execution_id, work_item_id, kind, status, title, body_markdown, created_at)
                 VALUES (?1, NULL, ?2, 'revision_parent_closed', 'open',
                         'Parent PR merged while revision was pending',
                         'The parent task''s PR was merged or closed before this revision could push its commit. The revision cannot be delivered. File a new chore (`boss task create`) to continue the work on the updated main branch.',
                         ?3)",
                params![attn_id, rev_id, now],
            )?;
        }
    }
    Ok(())
}
