use super::*;

impl WorkDb {
    /// Record that a worker produced a PR for `execution_id`. In a single
    /// transaction:
    ///   - the linked task/chore moves to the column dictated by
    ///     `target` (`in_review` for an open PR, `done` for a PR that
    ///     was already merged at Stop time) and gets `pr_url`
    ///     populated. If the task is already past the target column
    ///     (`done`, `archived`), its status is left alone — the
    ///     `pr_url` update still applies.
    ///   - the execution transitions from `waiting_human` (or `running`)
    ///     to `completed`, the cube workspace lease columns are
    ///     cleared, `finished_at` is stamped,
    ///   - the run summary is updated if a fresh summary is provided
    ///     and the run hasn't already captured one.
    ///
    /// Returns `Ok(None)` if the execution has already been finalised
    /// (terminal status), making this safe to call from a hook handler
    /// that may fire repeatedly.
    pub fn record_worker_pr_completion(
        &self,
        execution_id: &str,
        pr_url: &str,
        result_summary: Option<&str>,
        target: WorkerPrCompletionTarget,
    ) -> Result<Option<WorkerPrCompletion>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if execution_status_is_terminal(&execution.status) {
            return Ok(None);
        }
        if !matches!(execution.status.as_str(), "running" | "waiting_human") {
            bail!(
                "execution {execution_id} cannot complete from worker PR signal in status `{}`",
                execution.status
            );
        }

        let original_lease_id = execution.cube_lease_id.clone();
        let original_workspace_id = execution.cube_workspace_id.clone();

        let work_item_id = execution.work_item_id.clone();
        let task = query_task(&tx, &work_item_id)?
            .with_context(|| format!("unknown task for execution: {work_item_id}"))?;
        if task.deleted_at.is_some() {
            bail!("cannot complete a deleted task: {work_item_id}");
        }

        let now = now_string();
        // Compute the new status. The chore can only advance — if it
        // is already past the target column (`done` / `archived`), we
        // keep the existing status. `PendingReview` holds the task in
        // its current status so the reviewer pass runs before human Review.
        let new_status = match target {
            _ if task.status == "done" || task.status == "archived" => task.status.clone(),
            WorkerPrCompletionTarget::InReview if task.status == "in_review" => task.status.clone(),
            WorkerPrCompletionTarget::InReview => "in_review".to_owned(),
            WorkerPrCompletionTarget::Done => "done".to_owned(),
            // P992 task 7: hold in current status while the reviewer runs.
            WorkerPrCompletionTarget::PendingReview => task.status.clone(),
        };
        // Revision tasks do not own a PR — their `pr_url` must stay NULL
        // (the chain root's `pr_url` is the source of truth), *except* for
        // `PendingReview` where we must stamp it so the reviewer can find it.
        let pr_url_for_task: Option<&str> = match target {
            WorkerPrCompletionTarget::PendingReview => Some(pr_url),
            _ if task.kind == TaskKind::Revision => task.pr_url.as_deref(),
            _ => Some(pr_url),
        };
        tx.execute(
            "UPDATE tasks
             SET status             = ?2,
                 pr_url             = ?3,
                 updated_at         = ?4,
                 last_status_actor  = 'engine',
                 blocked_reason     = NULL,
                 blocked_attempt_id = NULL
             WHERE id = ?1",
            params![task.id, new_status, pr_url_for_task, now],
        )?;

        if new_status != task.status {
            cascade_dependents_after_prereq_status_change(&tx, &task.id, &new_status, &now)?;
        }

        tx.execute(
            "UPDATE work_executions
             SET status = 'completed',
                 cube_lease_id = NULL,
                 cube_workspace_id = NULL,
                 workspace_path = NULL,
                 finished_at = ?2,
                 pr_url = ?3
             WHERE id = ?1",
            params![execution_id, now, pr_url],
        )?;

        // Update the most-recent run for this execution: if a summary is
        // provided and the run's existing summary is empty, capture it.
        // The run is typically already `completed` because the
        // PaneSpawnRunner records completion immediately on spawn.
        if let Some(summary) = result_summary {
            let trimmed = summary.trim();
            if !trimmed.is_empty() {
                tx.execute(
                    "UPDATE work_runs
                     SET result_summary = COALESCE(NULLIF(result_summary, ''), ?2)
                     WHERE execution_id = ?1
                       AND id = (
                           SELECT id FROM work_runs
                           WHERE execution_id = ?1
                           ORDER BY created_at DESC, id DESC
                           LIMIT 1
                       )",
                    params![execution_id, trimmed],
                )?;
            }
        }

        let updated_execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        let updated_task = query_task(&tx, &work_item_id).require("task", &work_item_id)?;
        tx.commit()?;
        Ok(Some(WorkerPrCompletion {
            execution: updated_execution,
            work_item: task_to_item(updated_task),
            released_lease_id: original_lease_id,
            released_workspace_id: original_workspace_id,
        }))
    }

    /// Chores and project_tasks currently in `in_review` whose
    /// `pr_url` is set. The merge poller iterates this list, asks
    /// GitHub whether each PR is merged, and calls
    /// [`Self::mark_chore_pr_merged`] for the ones that are. Both
    /// kinds share the `pr_url` / `status='in_review'` shape, so the
    /// poller treats them identically; `kind = 'task'` is excluded
    /// deliberately because non-project tasks don't share the
    /// PR-on-merge lifecycle yet.
    pub fn list_chores_pending_merge_check(&self) -> Result<Vec<PendingMergeCheck>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, product_id, pr_url
             FROM tasks
             WHERE kind IN ('chore', 'project_task', 'design', 'investigation')
               AND status = 'in_review'
               AND pr_url IS NOT NULL
               AND pr_url != ''
               AND deleted_at IS NULL
             ORDER BY updated_at ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PendingMergeCheck {
                work_item_id: row.get(0)?,
                product_id: row.get(1)?,
                pr_url: row.get(2)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Executions whose bound chore is still `active` with no `pr_url`,
    /// whose execution row is `waiting_human` (i.e., the worker spawned,
    /// hit a Stop boundary, and is now idle), and that have a recorded
    /// `workspace_path` for PR detection.
    ///
    /// This is the fallback set for the merge poller's PR-open recheck:
    /// the on-Stop hook is the primary detection path but it can miss
    /// (transient `gh api` failure, GitHub's
    /// `commits/{sha}/pulls` index lagging a fresh `gh pr create`, or
    /// a Stop event that never reached the engine). Without this list
    /// the chore is stuck in `active` forever because the merge poller's
    /// other query (`list_chores_pending_merge_check`) only sees rows
    /// already in `in_review`.
    ///
    /// `kind IN ('chore', 'project_task', 'design')` matches the same
    /// kinds the in-review poller watches — `task` is excluded for the
    /// same reason (non-project tasks don't share the PR lifecycle).
    pub fn list_executions_pending_pr_detection(&self) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT we.id
             FROM work_executions we
             JOIN tasks t ON t.id = we.work_item_id
             WHERE we.status = 'waiting_human'
               AND we.workspace_path IS NOT NULL
               AND we.workspace_path != ''
               AND t.deleted_at IS NULL
               AND t.kind IN ('chore', 'project_task', 'design', 'investigation', 'revision')
               AND t.status = 'active'
               AND (t.pr_url IS NULL OR t.pr_url = '')
             ORDER BY we.created_at ASC",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        collect_rows(rows)
    }

    /// Return recently-terminal executions whose task is still `active`
    /// with no `pr_url`. These are candidates for the merge poller's
    /// late-PR-detection sweep (Bug B): when a double-spawn race causes
    /// exec_A to be abandoned before the real worker pushes its PR, the
    /// on-Stop hook returns `AlreadyTerminal` and the normal
    /// `list_executions_pending_pr_detection` query (which only watches
    /// `waiting_human`) never picks the chore back up. This query fills
    /// that gap by watching terminal executions that finished within the
    /// last `lookback_secs` seconds.
    ///
    /// Only executions with `workspace_path` set are returned — the
    /// absence of a workspace_path means the execution never reached the
    /// pane-spawn stage and therefore never pushed a branch the detector
    /// could find. Status `'cancelled'` and `'orphaned'` are excluded
    /// because those arise from human or engine actions that pre-date
    /// the pane-spawn lifecycle this sweep covers.
    pub fn list_recently_terminal_executions_pending_pr_detection(
        &self,
        lookback_secs: u64,
    ) -> Result<Vec<LatePrCandidate>> {
        let conn = self.connect()?;
        let cutoff = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(lookback_secs)
            .to_string();
        let mut stmt = conn.prepare(
            "SELECT we.id, we.work_item_id, we.repo_remote_url, we.branch_naming, we.worker_branch_prefix
             FROM work_executions we
             JOIN tasks t ON t.id = we.work_item_id
             WHERE we.status IN ('abandoned', 'completed', 'failed')
               AND we.workspace_path IS NOT NULL
               AND we.workspace_path != ''
               AND we.finished_at IS NOT NULL
               AND CAST(we.finished_at AS INTEGER) >= ?1
               AND t.deleted_at IS NULL
               AND t.kind IN ('chore', 'project_task', 'design', 'investigation')
               AND t.status = 'active'
               AND (t.pr_url IS NULL OR t.pr_url = '')
             ORDER BY we.finished_at DESC, we.id DESC",
        )?;
        let rows = stmt.query_map([cutoff], |row| {
            let branch_naming: BranchNaming = row
                .get::<_, Option<String>>(3)?
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            Ok(LatePrCandidate {
                execution_id: row.get(0)?,
                work_item_id: row.get(1)?,
                repo_remote_url: row.get(2)?,
                branch_naming,
                worker_branch_prefix: row
                    .get::<_, Option<String>>(4)?
                    .filter(|s| !s.is_empty()),
            })
        })?;
        collect_rows(rows)
    }

    /// Transition a task from `active` to `in_review` by binding a
    /// late-detected PR URL. Called by the merge poller's late-PR sweep
    /// when the PR was pushed after the original execution became
    /// terminal (double-spawn race). Unlike `record_worker_pr_completion`
    /// this function does not gate on execution status — the execution is
    /// already terminal; we only need to advance the task.
    ///
    /// Returns `Ok(true)` if the task was updated, `Ok(false)` if it was
    /// already past `active` (idempotent for concurrent sweeps).
    pub fn bind_pr_to_active_task_from_terminal_execution(
        &self,
        work_item_id: &str,
        pr_url: &str,
    ) -> Result<bool> {
        let conn = self.connect()?;
        let now = now_string();
        let rows_changed = conn.execute(
            "UPDATE tasks
             SET status            = 'in_review',
                 pr_url            = ?2,
                 updated_at        = ?3,
                 last_status_actor = 'engine',
                 blocked_reason    = NULL,
                 blocked_attempt_id = NULL
             WHERE id = ?1
               AND deleted_at IS NULL
               AND status = 'active'
               AND (pr_url IS NULL OR pr_url = '')",
            params![work_item_id, pr_url, now],
        )?;
        Ok(rows_changed > 0)
    }

    /// Move the chore or project_task identified by `work_item_id`
    /// from `in_review` to `done`, recording `pr_url` (no-op if it
    /// was already set to the same value). Returns the updated task
    /// if a transition happened; `Ok(None)` if the row was already
    /// past `in_review` (idempotent for late-arriving merge events).
    /// Callers are expected to pre-filter on `kind` via
    /// [`Self::list_chores_pending_merge_check`]; this function
    /// itself does not gate on kind so that the SQL filter remains
    /// the single source of truth for what's mergeable.
    pub fn mark_chore_pr_merged(&self, work_item_id: &str, pr_url: &str) -> Result<Option<Task>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let Some(task) = query_task(&tx, work_item_id)? else {
            return Ok(None);
        };
        if task.deleted_at.is_some() {
            return Ok(None);
        }
        if task.status == "done" || task.status == "archived" {
            return Ok(None);
        }
        let now = now_string();
        // Clearing blocked_reason / blocked_attempt_id is load-bearing
        // for the case where the merge poller observes a force-merge
        // (branch-protection override) of a PR currently in
        // `blocked: merge_conflict`. The new state must be coherent —
        // `done` rows never carry a blocked reason.
        tx.execute(
            "UPDATE tasks
             SET status             = 'done',
                 pr_url             = ?2,
                 updated_at         = ?3,
                 last_status_actor  = 'engine',
                 blocked_reason     = NULL,
                 blocked_attempt_id = NULL
             WHERE id = ?1",
            params![task.id, pr_url, now],
        )?;
        cascade_dependents_after_prereq_status_change(&tx, &task.id, "done", &now)?;
        // OQ7: when a chain root reaches `done`, flip any `in_review`
        // revisions on it to `done` as well.  A revision's deliverable
        // (the commit) rode the parent PR to its terminal state.
        flip_in_review_revisions_to_done(&tx, &task.id, &now)?;
        // Invalidation: any revision still in a pre-dispatch state
        // (todo / active / waiting_dependency / blocked-for-another-reason)
        // can never push to the merged PR.  Block them now so the
        // scheduler stops dispatching them and the kanban shows why.
        block_pending_revisions_on_parent_close(&tx, &task.id, &now)?;
        let updated = query_task(&tx, work_item_id)?
            .with_context(|| format!("unknown task after update: {work_item_id}"))?;
        tx.commit()?;
        Ok(Some(updated))
    }

    /// Update the PR poll-state columns for a single task row after a
    /// successful merge-poller probe. Stores the CI and review state strings
    /// (and optional JSON-encoded detail blobs) plus the current timestamp.
    ///
    /// Returns a [`PrPollStateOutcome`] carrying `changed` (the CI, review, or
    /// merge-queue state actually moved, so the caller should emit a change
    /// event) and `prior_ci_state` (the `ci_required_state` value stored
    /// *before* this update). `changed` is `false` when the probe confirmed
    /// the same state as before, or when the row was deleted / not found.
    /// Errors propagate from the underlying DB operations.
    ///
    /// The UPDATE is guarded by a `WHERE` clause that skips rows whose
    /// `ci_required_state` AND `review_required_state` are already set to
    /// the incoming values, so `changes() == 0` reliably means "nothing
    /// changed" — the caller does not need to issue a separate read.
    ///
    /// `prior_ci_state` is read in the same connection just before the UPDATE
    /// so the caller can detect a `fail → success` transition (CI recovered at
    /// the current head) and broadcast a `CiFailureCleared` event, reconciling
    /// a stale "ci failing" badge away during the poll we already do. Per-task
    /// poll writes are serialised by the sweep loop, so the read-then-write is
    /// race-free in practice.
    pub fn update_task_pr_poll_state(
        &self,
        work_item_id: &str,
        ci_required_state: &str,
        review_required_state: &str,
        ci_required_detail: Option<&str>,
        review_required_detail: Option<&str>,
        merge_queue_state: Option<&str>,
    ) -> Result<PrPollStateOutcome> {
        let conn = self.connect()?;
        let now = now_string();
        let prior_ci_state: Option<String> = conn
            .query_row(
                "SELECT ci_required_state FROM tasks
                 WHERE id = ?1 AND deleted_at IS NULL",
                params![work_item_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        // Only write (and count as changed) when the CI, review, or merge-queue
        // state differs from what's already stored. `COALESCE(col, '')` treats
        // NULL as distinct from any non-empty string, so the first probe after
        // migration always fires the event.
        let changed = conn.execute(
            "UPDATE tasks
             SET ci_required_state      = ?2,
                 review_required_state  = ?3,
                 ci_required_detail     = ?4,
                 review_required_detail = ?5,
                 pr_state_polled_at     = ?6,
                 merge_queue_state      = ?7
             WHERE id = ?1
               AND deleted_at IS NULL
               AND (COALESCE(ci_required_state, '') != ?2
                    OR COALESCE(review_required_state, '') != ?3
                    OR COALESCE(merge_queue_state, '') != COALESCE(?7, ''))",
            params![
                work_item_id,
                ci_required_state,
                review_required_state,
                ci_required_detail,
                review_required_detail,
                now,
                merge_queue_state,
            ],
        )?;
        Ok(PrPollStateOutcome {
            changed: changed > 0,
            prior_ci_state,
        })
    }
}

/// Outcome of [`WorkDb::update_task_pr_poll_state`].
#[derive(Debug, Clone)]
pub struct PrPollStateOutcome {
    /// `true` when the CI, review, or merge-queue state actually changed
    /// (so the caller should emit a `pr_poll_state_updated` event).
    pub changed: bool,
    /// The `ci_required_state` value stored *before* this update, or `None`
    /// when the column was NULL / the row was absent. Lets the caller detect
    /// a `fail → success` transition and clear a stale "ci failing" badge.
    pub prior_ci_state: Option<String>,
}

impl WorkDb {
    /// Return `(review_cycle, last_reviewed_sha)` for `task_id`.
    ///
    /// `review_cycle` is the number of `pr_review` passes that have completed
    /// for this task's PR. `last_reviewed_sha` is the PR HEAD SHA recorded at
    /// the end of the most recent pass, or `None` if no pass has completed yet.
    ///
    /// Used by the cycle-bound check in [`crate::completion::WorkerCompletionHandler`]
    /// before enqueuing a new `pr_review` execution. P992 design §7, task 9.
    pub fn get_task_review_cycle_state(
        &self,
        task_id: &str,
    ) -> Result<(i64, Option<String>)> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT review_cycle, last_reviewed_sha FROM tasks WHERE id = ?1",
            [task_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .with_context(|| format!("unknown task: {task_id}"))
    }

    /// Atomically increment `review_cycle` by 1 and set `last_reviewed_sha`.
    ///
    /// Called from [`crate::completion::WorkerCompletionHandler::finalize_pr_review_pass`]
    /// after a `pr_review` execution completes, regardless of whether a
    /// revision was warranted. A missing or empty `last_reviewed_sha` records
    /// `NULL` (the reviewer could not determine the HEAD SHA).
    /// P992 design §7, task 9.
    pub fn increment_task_review_cycle(
        &self,
        task_id: &str,
        last_reviewed_sha: Option<&str>,
    ) -> Result<()> {
        let conn = self.connect()?;
        let rows = conn.execute(
            "UPDATE tasks
             SET review_cycle      = review_cycle + 1,
                 last_reviewed_sha = ?2,
                 updated_at        = ?3
             WHERE id = ?1
               AND deleted_at IS NULL",
            params![
                task_id,
                last_reviewed_sha.filter(|s| !s.is_empty()),
                now_string(),
            ],
        )?;
        if rows == 0 {
            bail!("unknown or deleted task: {task_id}");
        }
        Ok(())
    }
}
