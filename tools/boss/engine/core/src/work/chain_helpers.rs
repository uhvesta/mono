use super::*;

/// Extract the numeric PR number from a GitHub pull-request URL.
/// Accepts the canonical form `https://github.com/<owner>/<repo>/pull/<n>`.
/// Returns `None` for any URL that does not end with a parseable integer.
fn extract_pr_number_from_url(pr_url: &str) -> Option<i64> {
    pr_url.rsplit('/').next().and_then(|s| s.parse::<i64>().ok())
}

/// `true` when `created_via` identifies the revision as an engine-managed
/// kind that is automatically resolved when the parent PR merges: a
/// merge-conflict-resolution revision or a CI-fix revision.
///
/// A conflicting PR cannot merge while still conflicted; a CI-failing PR
/// cannot merge while CI is failing — so by the time the parent PR merges,
/// those issues were already resolved.  Archiving the revision silently
/// avoids a dangling attention item or a spurious follow-up chore.
pub(crate) fn is_moot_revision_kind(created_via: &str) -> bool {
    created_via.starts_with(CREATED_VIA_MERGE_CONFLICT_PREFIX) || created_via.starts_with(CREATED_VIA_CI_FIX_PREFIX)
}

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
                 blocked_attempt_id = NULL,
                 completed_at      = COALESCE(completed_at, ?2)
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

/// Reconcile every pending revision in the chain when the parent PR merges
/// or closes externally.  The three cases handled here mirror the design doc:
///
/// 1. **Moot kinds** (`merge-conflict:*` / `ci-fix:*`) — the PR could not have
///    merged without those issues being resolved.  Archive silently; no
///    follow-up is needed.
///
/// 2. **Non-moot, active (WIP)** — the worker was mid-flight but the PR target
///    is gone.  Create a standalone chore with `autostart = true` so the work
///    is restarted on a fresh PR.  Archive the revision.  The in-flight
///    execution is stopped by the merge-poller's `stop_active_revision_executions`
///    call that runs immediately after this function returns.
///
/// 3. **Non-moot, in backlog** (`todo` / `blocked` for another reason) — the
///    work hasn't started yet.  Create a standalone chore with
///    `autostart = false` so the operator controls when it runs.  Archive the
///    revision.
///
/// `in_review` revisions are handled by [`flip_in_review_revisions_to_done`]
/// (their commit rode the PR to merge).  Terminal revisions (`done`,
/// `archived`, `cancelled`) and revisions already blocked with
/// `parent_pr_closed` are skipped.
pub(crate) fn block_pending_revisions_on_parent_close(conn: &Connection, chain_root_id: &str, now: &str) -> Result<()> {
    let revision_ids = collect_chain_revision_ids(conn, chain_root_id)?;
    if revision_ids.is_empty() {
        return Ok(());
    }
    for rev_id in &revision_ids {
        let Some(rev) = query_task(conn, rev_id)? else {
            continue;
        };
        if matches!(
            rev.status,
            TaskStatus::Done | TaskStatus::Archived | TaskStatus::Cancelled | TaskStatus::InReview
        ) {
            continue;
        }
        if rev.status == TaskStatus::Blocked && rev.blocked_reason.as_deref() == Some("parent_pr_closed") {
            continue;
        }

        if is_moot_revision_kind(&rev.created_via) {
            let rows_changed = conn.execute(
                "UPDATE tasks
                 SET status            = 'archived',
                     last_status_actor = 'engine',
                     updated_at        = ?2,
                     completed_at      = COALESCE(completed_at, ?2)
                 WHERE id = ?1
                   AND kind = 'revision'
                   AND deleted_at IS NULL",
                params![rev_id, now],
            )?;
            if rows_changed > 0 {
                tracing::info!(
                    revision_id = %rev_id,
                    chain_root_id,
                    created_via = %rev.created_via,
                    "block_pending_revisions: moot revision archived \
                     (merge-conflict/CI-fix; parent PR merged)",
                );
            }
        } else {
            let is_wip = rev.status == TaskStatus::Active;
            let is_pr_review = rev.created_via.starts_with(CREATED_VIA_PR_REVIEW_PREFIX);

            // For PR-review revisions, emit a `followup` with provenance.
            // For all other non-moot revisions, keep the historical `chore`.
            let (kind_override, origin_task_short_id, origin_pr_number, description) = if is_pr_review {
                let root = query_task(conn, chain_root_id)?;
                let origin_short_id = root.as_ref().and_then(|r| r.short_id);
                let origin_pr_num = root
                    .as_ref()
                    .and_then(|r| r.pr_url.as_deref().and_then(extract_pr_number_from_url));
                let desc = rev.description.replace(
                    "Address all findings before finalising this revision.",
                    "Address all findings before closing this follow-up.",
                );
                (Some(TaskKind::Followup), origin_short_id, origin_pr_num, desc)
            } else {
                (None, None, None, rev.description.clone())
            };

            let new_chore = insert_chore_in_tx(
                conn,
                CreateChoreInput::builder()
                    .product_id(rev.product_id.clone())
                    .autostart(is_wip)
                    .force_duplicate(true)
                    .name(rev.name.clone())
                    .maybe_created_via(Some(CREATED_VIA_ENGINE_AUTO.to_owned()))
                    .maybe_description(Some(description))
                    .maybe_effort_level(rev.effort_level)
                    .maybe_model_override(rev.model_override.clone())
                    .maybe_priority(Some(rev.priority.clone()))
                    .maybe_repo_remote_url(rev.repo_remote_url.clone())
                    .maybe_kind_override(kind_override)
                    .maybe_origin_task_short_id(origin_task_short_id)
                    .maybe_origin_pr_number(origin_pr_number)
                    .build(),
            )?;

            conn.execute(
                "UPDATE tasks
                 SET status            = 'archived',
                     last_status_actor = 'engine',
                     updated_at        = ?2,
                     completed_at      = COALESCE(completed_at, ?2)
                 WHERE id = ?1
                   AND kind = 'revision'
                   AND deleted_at IS NULL",
                params![rev_id, now],
            )?;
            tracing::info!(
                revision_id = %rev_id,
                new_chore_id = %new_chore.id,
                new_chore_kind = %new_chore.kind,
                chain_root_id,
                is_wip,
                is_pr_review,
                "block_pending_revisions: revision converted to standalone {} \
                 (parent PR merged; will {})",
                if is_pr_review { "followup" } else { "chore" },
                if is_wip { "auto-dispatch" } else { "stay in backlog" },
            );
        }
    }
    Ok(())
}
