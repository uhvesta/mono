use super::*;

impl WorkDb {
    /// Chores and project_tasks the engine previously flagged with
    /// `blocked: merge_conflict`. The merge poller iterates this list
    /// alongside [`Self::list_chores_pending_merge_check`] so that a
    /// PR returning to a mergeable state can be detected and the
    /// parent flipped back to `in_review` (design Q1's probe-pool
    /// extension).
    ///
    /// Same `PendingMergeCheck` shape as the in-review list so the
    /// poller can chain both iterators through one sweep loop.
    pub fn list_chores_blocked_on_merge_conflict(&self) -> Result<Vec<PendingMergeCheck>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, product_id, pr_url
             FROM tasks
             WHERE kind IN ('chore', 'project_task', 'design', 'investigation')
               AND status = 'blocked'
               AND blocked_reason = 'merge_conflict'
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

    /// Conflict-resolution attempts that are stranded: the parent task
    /// is `blocked: merge_conflict`, the `conflict_resolutions` row is
    /// `pending`, and no live execution (`kind='conflict_resolution'`
    /// AND `status IN ('ready','running','waiting_human')`) exists for
    /// that `work_item_id`. The merge poller's recovery sweep re-emits
    /// a fresh execution request for each of these so a worker can
    /// attempt the rebase.
    ///
    /// `abandoned` rows are excluded by the `status = 'pending'`
    /// filter — the churn guard (or a human) owns that path and those
    /// rows must not be automatically rescued.
    pub fn list_stranded_conflict_resolution_attempts(
        &self,
    ) -> Result<Vec<StrandedConflictAttempt>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT cr.id, cr.work_item_id, cr.product_id, cr.pr_url
             FROM conflict_resolutions cr
             WHERE cr.status = 'pending'
               AND EXISTS (
                   SELECT 1 FROM tasks t
                   WHERE t.id = cr.work_item_id
                     AND t.status = 'blocked'
                     AND t.blocked_reason = 'merge_conflict'
                     AND t.deleted_at IS NULL
               )
               AND NOT EXISTS (
                   SELECT 1 FROM work_executions we
                   WHERE we.work_item_id = cr.work_item_id
                     AND we.kind = 'conflict_resolution'
                     AND we.status IN ('ready', 'running', 'waiting_human')
               )
             ORDER BY cr.created_at ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(StrandedConflictAttempt {
                attempt_id: row.get(0)?,
                work_item_id: row.get(1)?,
                product_id: row.get(2)?,
                pr_url: row.get(3)?,
            })
        })?;
        collect_rows(rows)
    }

    /// CI-remediation attempts that are stranded: the parent task is
    /// `blocked: ci_failure`, the `ci_remediations` row is `pending`,
    /// and no live execution (`kind='ci_remediation'` AND
    /// `status IN ('ready','running','waiting_human')`) exists for that
    /// `work_item_id`. This occurs when two merge-queue dequeue events
    /// land in the same sweep: the first flips the task (consuming the
    /// `status='in_review'` WHERE guard on `mark_chore_blocked_ci_failure`)
    /// and the second inserts a new `ci_remediations` row but cannot flip
    /// the task again, leaving the row without an executor. The merge
    /// poller's recovery sweep re-emits a fresh execution request for
    /// each stranded row so a worker is dispatched.
    ///
    /// `ci_failure_exhausted` rows are excluded — the budget is spent and
    /// those tasks must not be automatically re-dispatched.
    pub fn list_stranded_ci_remediation_attempts(
        &self,
    ) -> Result<Vec<StrandedCiRemediationAttempt>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT cr.id, cr.work_item_id, cr.product_id, cr.pr_url
             FROM ci_remediations cr
             WHERE cr.status = 'pending'
               AND EXISTS (
                   SELECT 1 FROM tasks t
                   WHERE t.id = cr.work_item_id
                     AND t.status = 'blocked'
                     AND t.blocked_reason = 'ci_failure'
                     AND t.deleted_at IS NULL
               )
               AND NOT EXISTS (
                   SELECT 1 FROM work_executions we
                   WHERE we.work_item_id = cr.work_item_id
                     AND we.kind = 'ci_remediation'
                     AND we.status IN ('ready', 'running', 'waiting_human')
               )
             ORDER BY cr.created_at ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(StrandedCiRemediationAttempt {
                attempt_id: row.get(0)?,
                work_item_id: row.get(1)?,
                product_id: row.get(2)?,
                pr_url: row.get(3)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Chores and project_tasks the engine has flagged with either
    /// `blocked: ci_failure` or `blocked: ci_failure_exhausted`. The
    /// merge poller iterates this list alongside the in_review and
    /// merge-conflict-blocked lists so that:
    ///   - a still-`ci_failure` row can be observed for the symmetric
    ///     "CI went green again" transition, and
    ///   - a `ci_failure_exhausted` row is *also* probed, because the
    ///     user (or the provider) can clear the failure out from under
    ///     the engine and we want the parent to snap back to
    ///     `in_review` without manual intervention. Re-probing an
    ///     exhausted row does *not* re-fire the auto-fix flow (the
    ///     engine has given up); it only watches for the clear signal
    ///     (design §Q1 "Probe-pool extension").
    pub fn list_chores_blocked_on_ci_failure(&self) -> Result<Vec<PendingMergeCheck>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, product_id, pr_url
             FROM tasks
             WHERE kind IN ('chore', 'project_task', 'design', 'investigation')
               AND status = 'blocked'
               AND blocked_reason IN ('ci_failure', 'ci_failure_exhausted')
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

    /// WHERE-guarded flip of a chore/project_task from `in_review`
    /// to `blocked: merge_conflict`. Idempotent — a second call for
    /// a row already in this state updates zero rows and returns
    /// `Ok(None)`. Returns the updated task on the transition.
    ///
    /// The guard `status = 'in_review' AND pr_url = ?pr_url` is
    /// load-bearing: it prevents the engine from clobbering a row a
    /// human just moved elsewhere (e.g. manually back to `active`)
    /// or a PR that has been re-pointed at a different URL.
    pub fn mark_chore_blocked_merge_conflict(
        &self,
        work_item_id: &str,
        pr_url: &str,
    ) -> Result<Option<Task>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE tasks
                SET status            = 'blocked',
                    blocked_reason    = 'merge_conflict',
                    last_status_actor = 'engine',
                    updated_at        = ?3
              WHERE id = ?1
                AND status = 'in_review'
                AND pr_url = ?2
                AND deleted_at IS NULL",
            params![work_item_id, pr_url, now],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        // Phase 10 #31: keep the multi-signal side table in sync with
        // the scalar flip. `attempt_id` is `None` here because
        // `insert_conflict_resolution` runs immediately after this
        // method (it stamps `tasks.blocked_attempt_id`); the side
        // table's `attempt_id` is filled lazily by the attempt-insert
        // path below. The polymorphic clear dispatch only needs the
        // `reason` row to be present, so this stays correct even
        // before the attempt id lands.
        upsert_task_blocked_signal(&tx, work_item_id, "merge_conflict", None, &now)?;
        let updated = query_task(&tx, work_item_id)?
            .with_context(|| format!("unknown task after merge_conflict flip: {work_item_id}"))?;
        tx.commit()?;
        Ok(Some(updated))
    }

    /// Symmetric retire path: flip a chore/project_task currently
    /// `blocked: merge_conflict` back to `in_review` and clear the
    /// reason / attempt-id columns. Idempotent. Returns the updated
    /// task on the transition; `Ok(None)` when the WHERE clause
    /// missed (row already cleared, manually moved, or its PR url
    /// changed underneath us).
    pub fn clear_chore_blocked_merge_conflict(
        &self,
        work_item_id: &str,
        pr_url: &str,
    ) -> Result<Option<Task>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE tasks
                SET status             = 'in_review',
                    blocked_reason     = NULL,
                    blocked_attempt_id = NULL,
                    last_status_actor  = 'engine',
                    updated_at         = ?3
              WHERE id = ?1
                AND status = 'blocked'
                AND blocked_reason = 'merge_conflict'
                AND pr_url = ?2
                AND deleted_at IS NULL",
            params![work_item_id, pr_url, now],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        // Phase 10 #31: mark the matching side-table row(s) cleared so
        // the polymorphic clear dispatch doesn't re-fire on the next
        // probe. Mirrors `clear_chore_blocked_ci_failure`.
        tx.execute(
            "UPDATE task_blocked_signals
                SET cleared_at = ?2
              WHERE work_item_id = ?1
                AND reason = 'merge_conflict'
                AND cleared_at IS NULL",
            params![work_item_id, now],
        )?;
        let updated = query_task(&tx, work_item_id)?
            .with_context(|| format!("unknown task after merge_conflict clear: {work_item_id}"))?;
        tx.commit()?;
        Ok(Some(updated))
    }

    /// Stricter variant of [`Self::clear_chore_blocked_merge_conflict`]
    /// that also requires `blocked_attempt_id = ?attempt_id` in the
    /// WHERE clause (design Q5). Used by the auto-retire path when an
    /// engine-managed `conflict_resolutions` row exists for the
    /// transition: the attempt-id guard guarantees we only undo *our
    /// own* blocked rows, even if a human concurrently re-flipped the
    /// chore to a fresh `blocked: merge_conflict` under a different
    /// attempt id. Idempotent; returns `Ok(None)` on WHERE-guard miss.
    pub fn clear_chore_blocked_merge_conflict_for_attempt(
        &self,
        work_item_id: &str,
        pr_url: &str,
        attempt_id: &str,
    ) -> Result<Option<Task>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE tasks
                SET status             = 'in_review',
                    blocked_reason     = NULL,
                    blocked_attempt_id = NULL,
                    last_status_actor  = 'engine',
                    updated_at         = ?4
              WHERE id = ?1
                AND status = 'blocked'
                AND blocked_reason = 'merge_conflict'
                AND pr_url = ?2
                AND blocked_attempt_id = ?3
                AND deleted_at IS NULL",
            params![work_item_id, pr_url, attempt_id, now],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        // Phase 10 #31: keep the side table in sync (see the relaxed
        // sibling [`Self::clear_chore_blocked_merge_conflict`]).
        tx.execute(
            "UPDATE task_blocked_signals
                SET cleared_at = ?2
              WHERE work_item_id = ?1
                AND reason = 'merge_conflict'
                AND cleared_at IS NULL",
            params![work_item_id, now],
        )?;
        let updated = query_task(&tx, work_item_id)?
            .with_context(|| format!("unknown task after merge_conflict clear: {work_item_id}"))?;
        tx.commit()?;
        Ok(Some(updated))
    }

    /// WHERE-guarded flip of a chore/project_task from `in_review` to
    /// `blocked: ci_failure`. Mirrors
    /// [`Self::mark_chore_blocked_merge_conflict`] but for the CI
    /// signal — idempotent against second probes, gated on the row
    /// still being `in_review` for the same `pr_url`. Returns the
    /// updated task on transition; `Ok(None)` when the guard misses
    /// (row already blocked or moved by a human).
    ///
    /// `task_blocked_signals` is upserted with the matching
    /// `('ci_failure', attempt_id)` row so the multi-signal view
    /// stays in sync. The scalar `blocked_reason` cache is set to
    /// `'ci_failure'` only when no higher-priority signal already
    /// occupies the slot — the design's §Q2 priority order is
    /// (dependency > review_feedback > merge_conflict >
    /// ci_failure_exhausted > ci_failure).
    pub fn mark_chore_blocked_ci_failure(
        &self,
        work_item_id: &str,
        pr_url: &str,
        attempt_id: Option<&str>,
    ) -> Result<Option<Task>> {
        self.mark_chore_blocked_ci_signal(work_item_id, pr_url, attempt_id, "ci_failure")
    }

    /// Variant of [`Self::mark_chore_blocked_ci_failure`] for the
    /// budget-exhausted exit. Same WHERE guard but the
    /// `blocked_reason` scalar lands as `'ci_failure_exhausted'` (the
    /// UI surface for "engine has given up; please intervene"). The
    /// side-table row carries `reason='ci_failure_exhausted'` too so
    /// the multi-signal projection stays consistent.
    ///
    /// Idempotent for both the in_review → exhausted and the
    /// ci_failure → exhausted transitions — the WHERE clause matches
    /// either as long as the parent isn't already exhausted, the row
    /// hasn't been deleted, and the PR url still matches.
    pub fn mark_chore_blocked_ci_failure_exhausted(
        &self,
        work_item_id: &str,
        pr_url: &str,
    ) -> Result<Option<Task>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        // Match either `in_review` (first failure, budget already 0) or
        // an active `ci_failure` row whose budget has now exhausted.
        let rows = tx.execute(
            "UPDATE tasks
                SET status            = 'blocked',
                    blocked_reason    = 'ci_failure_exhausted',
                    last_status_actor = 'engine',
                    updated_at        = ?3
              WHERE id = ?1
                AND pr_url = ?2
                AND deleted_at IS NULL
                AND (
                       status = 'in_review'
                    OR (status = 'blocked' AND blocked_reason = 'ci_failure')
                )",
            params![work_item_id, pr_url, now],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        upsert_task_blocked_signal(&tx, work_item_id, "ci_failure_exhausted", None, &now)?;
        let updated = query_task(&tx, work_item_id)?.with_context(|| {
            format!("unknown task after ci_failure_exhausted flip: {work_item_id}")
        })?;
        tx.commit()?;
        Ok(Some(updated))
    }

    pub(crate) fn mark_chore_blocked_ci_signal(
        &self,
        work_item_id: &str,
        pr_url: &str,
        attempt_id: Option<&str>,
        reason: &str,
    ) -> Result<Option<Task>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE tasks
                SET status             = 'blocked',
                    blocked_reason     = ?4,
                    blocked_attempt_id = COALESCE(?3, blocked_attempt_id),
                    last_status_actor  = 'engine',
                    updated_at         = ?5
              WHERE id = ?1
                AND status = 'in_review'
                AND pr_url = ?2
                AND deleted_at IS NULL",
            params![work_item_id, pr_url, attempt_id, reason, now],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        upsert_task_blocked_signal(&tx, work_item_id, reason, attempt_id, &now)?;
        let updated = query_task(&tx, work_item_id)?
            .with_context(|| format!("unknown task after {reason} flip: {work_item_id}"))?;
        tx.commit()?;
        Ok(Some(updated))
    }

    /// Symmetric CI retire path: flip a chore/project_task currently
    /// `blocked: ci_failure` (or `ci_failure_exhausted`) back to
    /// `in_review`, clear the reason / attempt-id columns, and stamp
    /// the matching `task_blocked_signals` rows as `cleared_at`.
    /// Idempotent — returns `Ok(None)` on WHERE-guard miss.
    pub fn clear_chore_blocked_ci_failure(
        &self,
        work_item_id: &str,
        pr_url: &str,
    ) -> Result<Option<Task>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE tasks
                SET status             = 'in_review',
                    blocked_reason     = NULL,
                    blocked_attempt_id = NULL,
                    last_status_actor  = 'engine',
                    updated_at         = ?3
              WHERE id = ?1
                AND status = 'blocked'
                AND blocked_reason IN ('ci_failure', 'ci_failure_exhausted')
                AND pr_url = ?2
                AND deleted_at IS NULL",
            params![work_item_id, pr_url, now],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        tx.execute(
            "UPDATE task_blocked_signals
                SET cleared_at = ?2
              WHERE work_item_id = ?1
                AND reason IN ('ci_failure', 'ci_failure_exhausted')
                AND cleared_at IS NULL",
            params![work_item_id, now],
        )?;
        let updated = query_task(&tx, work_item_id)?
            .with_context(|| format!("unknown task after ci_failure clear: {work_item_id}"))?;
        tx.commit()?;
        Ok(Some(updated))
    }

    /// Effective CI attempt budget for `work_item_id`: per-PR override
    /// when set, falling back to the parent product's default (and
    /// finally the documented default of 3 if neither row carries a
    /// value). Capped at the documented hard limit of 10 to prevent a
    /// misconfigured product from spinning forever (design §Q3).
    pub fn effective_ci_budget(&self, work_item_id: &str) -> Result<i64> {
        let conn = self.connect()?;
        let raw: Option<(Option<i64>, i64)> = conn
            .query_row(
                "SELECT t.ci_attempt_budget,
                        COALESCE(p.ci_attempt_budget, 3) AS product_budget
                 FROM tasks t
                 JOIN products p ON p.id = t.product_id
                 WHERE t.id = ?1",
                params![work_item_id],
                |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((per_pr, product_default)) = raw else {
            return Ok(3);
        };
        let effective = per_pr.unwrap_or(product_default);
        Ok(effective.clamp(0, 10))
    }

    /// Snapshot of every active `task_blocked_signals` row for one
    /// work item. "Active" means `cleared_at IS NULL` — flapping
    /// signals are reset to active on re-observation via
    /// [`upsert_task_blocked_signal`] so a row only stays active while
    /// the underlying condition is still observed.
    ///
    /// Returned in `created_at ASC` order so the polymorphic clear
    /// path in the merge poller iterates oldest-first (mirrors the
    /// design's `maybe_clear_blocked` snippet — order doesn't affect
    /// correctness because each signal clears against its own probe
    /// condition, but the deterministic ordering keeps the log /
    /// activity-feed trail predictable).
    pub fn active_blocked_signals(&self, work_item_id: &str) -> Result<Vec<BlockedSignal>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT work_item_id, reason, attempt_id, created_at, cleared_at
             FROM task_blocked_signals
             WHERE work_item_id = ?1
               AND cleared_at IS NULL
             ORDER BY created_at ASC, reason ASC",
        )?;
        let rows = stmt.query_map([work_item_id], |row| {
            Ok(BlockedSignal {
                work_item_id: row.get(0)?,
                reason: row.get(1)?,
                attempt_id: row.get(2)?,
                created_at: row.get(3)?,
                cleared_at: row.get(4)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Re-upsert the `task_blocked_signals` row for `merge_conflict` when the
    /// parent task is already `blocked: merge_conflict` but the signal row
    /// was cleared (e.g. by the polymorphic-clear path that ran prematurely
    /// against a stale probe — T230 scenario).
    ///
    /// Returns `true` if the task IS `blocked: merge_conflict` (and the
    /// signal was upserted); `false` when the task is not in that state, which
    /// lets the caller distinguish a "human moved the row" miss from the
    /// stale-crz re-arm scenario. A `false` return means the caller should
    /// leave the row alone.
    pub fn rearm_blocked_merge_conflict_signal(&self, work_item_id: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let is_blocked: bool = tx
            .query_row(
                "SELECT 1 FROM tasks
                 WHERE id = ?1
                   AND status = 'blocked'
                   AND blocked_reason = 'merge_conflict'
                   AND deleted_at IS NULL",
                params![work_item_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !is_blocked {
            tx.commit()?;
            return Ok(false);
        }
        upsert_task_blocked_signal(&tx, work_item_id, "merge_conflict", None, &now)?;
        tx.commit()?;
        Ok(true)
    }

    /// Read `tasks.blocked_reason` for a task currently in `status='blocked'`.
    /// Returns `Ok(None)` when the task is not blocked (or is soft-deleted,
    /// or does not exist). Used by the merge-poller's drift-guard fallback in
    /// `maybe_clear_blocked` to drive the retire path even when
    /// `task_blocked_signals` is empty (T230-style inconsistency).
    pub fn task_blocked_reason(&self, work_item_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT blocked_reason FROM tasks
             WHERE id = ?1
               AND status = 'blocked'
               AND blocked_reason IS NOT NULL
               AND deleted_at IS NULL",
            params![work_item_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Read the current `ci_attempts_used` counter for a work item.
    /// Defaults to 0 when the row or column is missing (the budget
    /// kicks in only when the parent first enters the CI-failure
    /// flow, so legacy in-flight rows return 0 here).
    pub fn get_ci_attempts_used(&self, work_item_id: &str) -> Result<i64> {
        let conn = self.connect()?;
        let used: Option<i64> = conn
            .query_row(
                "SELECT ci_attempts_used FROM tasks WHERE id = ?1",
                params![work_item_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        Ok(used.unwrap_or(0))
    }

    /// Increment the `ci_attempts_used` counter for `work_item_id` by
    /// one. Used by the CI-watch detect path when a fix attempt
    /// progresses past the worker's go/no-go (design §Q3 "what counts
    /// as one attempt"). Idempotent only insofar as the unique key on
    /// `ci_remediations` prevents the same `(work_item, head_sha, kind)`
    /// from incrementing twice — callers are expected to bump only
    /// when an insert actually produced a fresh row.
    pub fn increment_ci_attempts_used(&self, work_item_id: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE tasks
                SET ci_attempts_used = ci_attempts_used + 1
              WHERE id = ?1
                AND deleted_at IS NULL",
            params![work_item_id],
        )?;
        Ok(())
    }

    /// Reset `ci_attempts_used` to 0 for `work_item_id`. Called by
    /// the CI-watch retire path on a successful cycle (design §Q3
    /// "Budget reset rules"). Idempotent.
    pub fn reset_ci_attempts_used(&self, work_item_id: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE tasks
                SET ci_attempts_used = 0
              WHERE id = ?1
                AND deleted_at IS NULL",
            params![work_item_id],
        )?;
        Ok(())
    }

    /// Insert a `ci_remediations` row with `status='pending'`.
    /// Mirrors [`Self::insert_conflict_resolution`] but for the CI
    /// signal: the unique key is `(work_item_id, head_sha_at_trigger,
    /// attempt_kind)` and the engine uses `INSERT OR IGNORE` so a
    /// second probe for the same triplet is a no-op (caller reads the
    /// existing row separately). `failed_checks` is the JSON-encoded
    /// snapshot the engine captured at trigger time; `consumes_budget`
    /// must be `1` for `attempt_kind='fix'` and `0` for `'retrigger'`.
    /// Phase 9 ships the worker-spawn wiring; this method is the
    /// Phase 8 detection-side seam used by `ci_watch`.
    pub fn insert_ci_remediation(
        &self,
        input: CiRemediationInsertInput,
    ) -> Result<Option<CiRemediation>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let id = next_id("cir");
        let now = now_string();
        let rows = tx.execute(
            "INSERT OR IGNORE INTO ci_remediations
                (id, product_id, work_item_id, pr_url, pr_number,
                 head_branch, head_sha_at_trigger, attempt_kind,
                 consumes_budget, failed_checks, status, created_at,
                 failure_kind, before_commit_sha)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending', ?11, ?12, ?13)",
            params![
                id,
                input.product_id,
                input.work_item_id,
                input.pr_url,
                input.pr_number,
                input.head_branch,
                input.head_sha_at_trigger,
                input.attempt_kind,
                input.consumes_budget,
                input.failed_checks,
                now,
                input.failure_kind,
                input.before_commit_sha,
            ],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        let inserted = query_ci_remediation(&tx, &id)?
            .with_context(|| format!("unknown ci_remediation after insert: {id}"))?;
        tx.commit()?;
        Ok(Some(inserted))
    }

    /// Read a `ci_remediations` row by id, terminal or not. Returns
    /// `Ok(None)` for an unknown id. Used by the worker-marker
    /// handlers in `app.rs` to echo the post-update row back to the
    /// CLI and to snapshot pre-flip state for refund decisions.
    pub fn get_ci_remediation(&self, attempt_id: &str) -> Result<Option<CiRemediation>> {
        let conn = self.connect()?;
        query_ci_remediation(&conn, attempt_id)
    }

    /// Read-only list of `ci_remediations` rows for `boss engine ci
    /// list` (design Phase 11 #35). Mirror of
    /// [`Self::list_conflict_resolutions`]. Filters are AND-ed; an
    /// empty `status` slice means "any status." Rows come back freshest
    /// first (`created_at DESC, id DESC`); `limit = None` returns every
    /// match — the CLI applies its own default cap.
    pub fn list_ci_remediations(
        &self,
        product_id: Option<&str>,
        statuses: &[String],
        work_item_id: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<CiRemediation>> {
        let conn = self.connect()?;
        let mut sql = String::from(
            "SELECT id, product_id, work_item_id, pr_url, pr_number,
                    head_branch, head_sha_at_trigger, head_sha_after,
                    attempt_kind, consumes_budget, failed_checks,
                    triage_class, log_excerpt, status, failure_reason,
                    cube_lease_id, cube_workspace_id, worker_id,
                    created_at, started_at, finished_at,
                    failure_kind, before_commit_sha, revision_task_id
             FROM ci_remediations WHERE 1=1",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(pid) = product_id {
            sql.push_str(" AND product_id = ?");
            params_vec.push(Box::new(pid.to_owned()));
        }
        if let Some(wid) = work_item_id {
            sql.push_str(" AND work_item_id = ?");
            params_vec.push(Box::new(wid.to_owned()));
        }
        if !statuses.is_empty() {
            sql.push_str(" AND status IN (");
            for (idx, status) in statuses.iter().enumerate() {
                if idx > 0 {
                    sql.push(',');
                }
                sql.push('?');
                params_vec.push(Box::new(status.clone()));
            }
            sql.push(')');
        }
        sql.push_str(" ORDER BY created_at DESC, id DESC");
        if let Some(cap) = limit {
            sql.push_str(" LIMIT ?");
            params_vec.push(Box::new(cap as i64));
        }
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|b| b.as_ref() as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt.query_map(refs.as_slice(), map_ci_remediation)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// User-facing `boss engine ci retry` action — design Phase 11
    /// #35 / Q11 "manual escape hatch." Two side effects:
    ///
    /// 1. Reset `tasks.ci_attempts_used` to 0 so the next probe is not
    ///    gated by the budget.
    /// 2. When the parent is currently `blocked: ci_failure_exhausted`,
    ///    flip it back to `in_review` (the next merge-poller sweep
    ///    re-fires the auto-fix flow on the still-failing CI). The
    ///    matching `task_blocked_signals` row is stamped `cleared_at`.
    ///
    /// Returns the post-update [`CiBudgetSnapshot`] and a flag for
    /// whether the parent had actually been in the exhausted state at
    /// the time of the call (so the CLI can render "now unblocked"
    /// vs "counter reset only"). `Ok(None)` when `work_item_id` does
    /// not exist (or has been soft-deleted).
    pub fn retry_ci_remediation_for_work_item(
        &self,
        work_item_id: &str,
    ) -> Result<Option<(CiBudgetSnapshot, bool)>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        // Confirm the parent exists (and capture its current
        // blocked_reason). A missing parent → Ok(None) so the CLI can
        // surface "unknown work item" without conflating with the
        // counter-reset-only path.
        let row: Option<(Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT status, blocked_reason FROM tasks
                  WHERE id = ?1 AND deleted_at IS NULL",
                params![work_item_id],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()?;
        let Some((status, blocked_reason)) = row else {
            tx.commit()?;
            return Ok(None);
        };
        let now = now_string();
        // Always reset the counter — repeated retry calls are
        // idempotent at the counter level.
        tx.execute(
            "UPDATE tasks
                SET ci_attempts_used = 0,
                    updated_at       = ?2
              WHERE id = ?1
                AND deleted_at IS NULL",
            params![work_item_id, now],
        )?;
        let was_exhausted = matches!(
            (status.as_deref(), blocked_reason.as_deref()),
            (Some("blocked"), Some("ci_failure_exhausted"))
        );
        if was_exhausted {
            // Clear the parent's exhaustion: status → in_review, drop
            // blocked_reason / blocked_attempt_id. The merge-poller's
            // next sweep observes CI=Failing and re-enters the
            // detection flow naturally.
            tx.execute(
                "UPDATE tasks
                    SET status             = 'in_review',
                        blocked_reason     = NULL,
                        blocked_attempt_id = NULL,
                        last_status_actor  = 'engine',
                        updated_at         = ?2
                  WHERE id = ?1
                    AND status = 'blocked'
                    AND blocked_reason = 'ci_failure_exhausted'
                    AND deleted_at IS NULL",
                params![work_item_id, now],
            )?;
            // Clear the matching blocked-signal row so the side table
            // reflects the parent's transition out of the blocked set.
            tx.execute(
                "UPDATE task_blocked_signals
                    SET cleared_at = ?2
                  WHERE work_item_id = ?1
                    AND reason = 'ci_failure_exhausted'
                    AND cleared_at IS NULL",
                params![work_item_id, now],
            )?;
        }
        // Compute the post-update budget snapshot for the response.
        let per_pr_override: Option<i64> = tx
            .query_row(
                "SELECT ci_attempt_budget FROM tasks WHERE id = ?1",
                params![work_item_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .unwrap_or(None);
        let product_default: i64 = tx
            .query_row(
                "SELECT COALESCE(p.ci_attempt_budget, 3)
                   FROM tasks t
                   JOIN products p ON p.id = t.product_id
                  WHERE t.id = ?1",
                params![work_item_id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(3);
        let used: i64 = tx
            .query_row(
                "SELECT ci_attempts_used FROM tasks WHERE id = ?1",
                params![work_item_id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0);
        let post_blocked_reason: Option<String> = tx
            .query_row(
                "SELECT blocked_reason FROM tasks
                  WHERE id = ?1
                    AND status = 'blocked'
                    AND deleted_at IS NULL",
                params![work_item_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap_or(None);
        tx.commit()?;
        let effective = per_pr_override.unwrap_or(product_default).clamp(0, 10);
        Ok(Some((
            CiBudgetSnapshot {
                work_item_id: work_item_id.to_owned(),
                per_pr_override,
                product_default,
                effective,
                used,
                blocked_reason: post_blocked_reason,
            },
            was_exhausted,
        )))
    }

    /// Read the current [`CiBudgetSnapshot`] for `work_item_id`.
    /// Returns `Ok(None)` when the row is missing (or soft-deleted).
    /// Used by both the `boss engine ci budget show` verb and the
    /// retry path's response.
    pub fn ci_budget_snapshot(&self, work_item_id: &str) -> Result<Option<CiBudgetSnapshot>> {
        let conn = self.connect()?;
        let row: Option<(Option<i64>, i64, i64, Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT t.ci_attempt_budget,
                        COALESCE(p.ci_attempt_budget, 3) AS product_default,
                        t.ci_attempts_used,
                        t.status,
                        t.blocked_reason
                 FROM tasks t
                 JOIN products p ON p.id = t.product_id
                 WHERE t.id = ?1
                   AND t.deleted_at IS NULL",
                params![work_item_id],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((per_pr_override, product_default, used, status, blocked_reason)) = row else {
            return Ok(None);
        };
        let effective = per_pr_override.unwrap_or(product_default).clamp(0, 10);
        let blocked_reason = match status.as_deref() {
            Some("blocked") => blocked_reason,
            _ => None,
        };
        Ok(Some(CiBudgetSnapshot {
            work_item_id: work_item_id.to_owned(),
            per_pr_override,
            product_default,
            effective,
            used,
            blocked_reason,
        }))
    }

    /// Set (or clear) `tasks.ci_attempt_budget` for `work_item_id`.
    /// `None` clears the override (the product default applies).
    /// `Some(n)` is clamped server-side to `0..=10` per the design's
    /// reserved range. Returns the post-update [`CiBudgetSnapshot`];
    /// `Ok(None)` when the work item does not exist.
    pub fn set_ci_attempt_budget(
        &self,
        work_item_id: &str,
        budget: Option<i64>,
    ) -> Result<Option<CiBudgetSnapshot>> {
        let conn = self.connect()?;
        let clamped = budget.map(|b| b.clamp(0, 10));
        let now = now_string();
        let rows = conn.execute(
            "UPDATE tasks
                SET ci_attempt_budget = ?2,
                    updated_at        = ?3
              WHERE id = ?1
                AND deleted_at IS NULL",
            params![work_item_id, clamped, now],
        )?;
        if rows == 0 {
            return Ok(None);
        }
        self.ci_budget_snapshot(work_item_id)
    }

    /// Unified [`EngineAttemptListEntry`] projection across the three
    /// attempt subsystems (`conflict_resolutions`, `rebase_attempts`,
    /// `ci_remediations`). Filters are AND-ed; `kinds` is the set of
    /// kind discriminators to include (empty == all three);
    /// `statuses` and `work_item_id` apply within each kind's own
    /// schema. Backs `boss engine attempts list` (design Phase 11 #36).
    ///
    /// `rebase_attempts` is the auto-rebase flow's table — it is
    /// guarded with `table_exists` because its DDL ships with
    /// `auto-rebase-stacked-prs.md`, which has not landed at the time
    /// this method does. When the table isn't there, the rebase kind
    /// silently contributes zero rows.
    pub fn list_engine_attempts(
        &self,
        kinds: &[String],
        product_id: Option<&str>,
        statuses: &[String],
        work_item_id: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<EngineAttemptListEntry>> {
        let want_conflict = kinds.is_empty() || kinds.iter().any(|k| k == "conflict");
        let want_rebase = kinds.is_empty() || kinds.iter().any(|k| k == "rebase");
        let want_ci = kinds.is_empty() || kinds.iter().any(|k| k == "ci");
        let mut out: Vec<EngineAttemptListEntry> = Vec::new();
        if want_conflict {
            for c in self.list_conflict_resolutions(product_id, statuses, work_item_id, None)? {
                out.push(EngineAttemptListEntry {
                    kind: "conflict".into(),
                    id: c.id,
                    product_id: c.product_id,
                    work_item_id: Some(c.work_item_id),
                    pr_url: c.pr_url,
                    status: c.status,
                    failure_reason: c.failure_reason,
                    created_at: c.created_at,
                    started_at: c.started_at,
                    finished_at: c.finished_at,
                    extra: Default::default(),
                });
            }
        }
        if want_ci {
            for r in self.list_ci_remediations(product_id, statuses, work_item_id, None)? {
                let mut extra = std::collections::BTreeMap::new();
                extra.insert("attempt_kind".into(), r.attempt_kind.clone());
                out.push(EngineAttemptListEntry {
                    kind: "ci".into(),
                    id: r.id,
                    product_id: r.product_id,
                    work_item_id: Some(r.work_item_id),
                    pr_url: r.pr_url,
                    status: r.status,
                    failure_reason: r.failure_reason,
                    created_at: r.created_at,
                    started_at: r.started_at,
                    finished_at: r.finished_at,
                    extra,
                });
            }
        }
        if want_rebase {
            let conn = self.connect()?;
            if table_exists(&conn, "rebase_attempts")? {
                // The rebase_attempts schema is established by the
                // auto-rebase-stacked-prs flow. We project a minimal
                // set of columns matching what we project for the
                // other kinds, tolerating columns that may not exist
                // yet by reading defensively. Today only the test
                // shim's DDL is in the tree (id / dependent_pr_url /
                // status); the production table will add timestamps
                // and product_id.
                let mut sql = String::from(
                    "SELECT id,
                            COALESCE(product_id, '') AS product_id,
                            dependent_pr_url AS pr_url,
                            status,
                            COALESCE(failure_reason, NULL) AS failure_reason,
                            COALESCE(created_at, '') AS created_at,
                            started_at,
                            finished_at
                     FROM rebase_attempts WHERE 1=1",
                );
                let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
                if let Some(pid) = product_id {
                    sql.push_str(" AND product_id = ?");
                    params_vec.push(Box::new(pid.to_owned()));
                }
                if !statuses.is_empty() {
                    sql.push_str(" AND status IN (");
                    for (idx, status) in statuses.iter().enumerate() {
                        if idx > 0 {
                            sql.push(',');
                        }
                        sql.push('?');
                        params_vec.push(Box::new(status.clone()));
                    }
                    sql.push(')');
                }
                sql.push_str(" ORDER BY created_at DESC, id DESC");
                // Use a sub-scope so the prepared statement borrow
                // ends before we move `conn` again.
                let refs: Vec<&dyn rusqlite::ToSql> = params_vec
                    .iter()
                    .map(|b| b.as_ref() as &dyn rusqlite::ToSql)
                    .collect();
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(refs.as_slice(), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                    ))
                })?;
                for r in rows {
                    let (id, pid, pr_url, status, fr, created_at, started_at, finished_at) = r?;
                    out.push(EngineAttemptListEntry {
                        kind: "rebase".into(),
                        id,
                        product_id: pid,
                        // rebase_attempts is keyed on PR URL today;
                        // no work_item_id projection yet.
                        work_item_id: None,
                        pr_url,
                        status,
                        failure_reason: fr,
                        created_at,
                        started_at,
                        finished_at,
                        extra: Default::default(),
                    });
                }
            }
        }
        // Merge by created_at DESC so the unified list is freshest-first.
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        if let Some(cap) = limit {
            out.truncate(cap as usize);
        }
        Ok(out)
    }

    /// Latest non-terminal `ci_remediations` row for `work_item_id`,
    /// or `None`. Used by `ci_watch` to detect "an attempt is already
    /// in flight" and by the retire path to find the row to flip to
    /// `succeeded` when the next probe reports CI back at clean.
    pub fn active_ci_remediation_for_work_item(
        &self,
        work_item_id: &str,
    ) -> Result<Option<CiRemediation>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, product_id, work_item_id, pr_url, pr_number,
                    head_branch, head_sha_at_trigger, head_sha_after,
                    attempt_kind, consumes_budget, failed_checks,
                    triage_class, log_excerpt, status, failure_reason,
                    cube_lease_id, cube_workspace_id, worker_id,
                    created_at, started_at, finished_at,
                    failure_kind, before_commit_sha, revision_task_id
             FROM ci_remediations
             WHERE work_item_id = ?1
               AND status IN ('pending', 'running')
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
        )?;
        let mut rows = stmt.query_map([work_item_id], map_ci_remediation)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Does the work item have a `ci_failure_suppressions` row for
    /// `head_sha`? Set by manual moves out of `blocked: ci_failure`
    /// to keep the next probe from immediately re-flipping the row
    /// (design §Q5 manual-override behaviour). The suppression is
    /// scoped to one head sha — a fresh push invalidates it.
    pub fn is_ci_failure_suppressed(&self, work_item_id: &str, head_sha: &str) -> Result<bool> {
        let conn = self.connect()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM ci_failure_suppressions
              WHERE work_item_id = ?1 AND head_sha = ?2",
            params![work_item_id, head_sha],
            |row| row.get(0),
        )?;
        Ok(n > 0)
    }

    /// Record an `InFlight` observation for `(work_item_id, head_sha)`
    /// and return the row that is now durably persisted. On the first
    /// observation for the pair, `first_observed_at` is stamped to
    /// `now`; subsequent calls find the existing row and leave it
    /// alone (so elapsed-time math always reads from the *first*
    /// observation, never the most recent). Per design Phase 12 #39.
    pub fn observe_ci_in_flight(
        &self,
        work_item_id: &str,
        head_sha: &str,
    ) -> Result<CiInFlightObservation> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        tx.execute(
            "INSERT OR IGNORE INTO ci_inflight_observations
                 (work_item_id, head_sha, first_observed_at, alert_level_emitted)
             VALUES (?1, ?2, ?3, 'none')",
            params![work_item_id, head_sha, now],
        )?;
        let row: (String, String) = tx.query_row(
            "SELECT first_observed_at, alert_level_emitted
               FROM ci_inflight_observations
              WHERE work_item_id = ?1 AND head_sha = ?2",
            params![work_item_id, head_sha],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        tx.commit()?;
        Ok(CiInFlightObservation {
            work_item_id: work_item_id.to_owned(),
            head_sha: head_sha.to_owned(),
            first_observed_at: row.0,
            alert_level_emitted: row.1,
        })
    }

    /// Record that the engine emitted a never-starts alert at `level`
    /// for the row keyed by `(work_item_id, head_sha)`. Levels are
    /// monotonic in observation time (`none → warn → alert`); callers
    /// pass the level they're emitting *now* and the WHERE guard
    /// ensures we never downgrade. The write is idempotent — a
    /// repeated emit of the same level is a no-op.
    pub fn mark_ci_inflight_alert_level(
        &self,
        work_item_id: &str,
        head_sha: &str,
        level: &str,
    ) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE ci_inflight_observations
                SET alert_level_emitted = ?3
              WHERE work_item_id = ?1
                AND head_sha = ?2
                AND alert_level_emitted != ?3
                AND (alert_level_emitted = 'none'
                     OR (alert_level_emitted = 'warn' AND ?3 = 'alert'))",
            params![work_item_id, head_sha, level],
        )?;
        Ok(())
    }

    /// Drop any `ci_inflight_observations` rows for `work_item_id`.
    /// Called from the CI-watch detect (Failing) and retire (Clean)
    /// paths so an in-flight observation row doesn't linger past the
    /// state the alert was tracking. Cheap — typically zero rows.
    pub fn clear_ci_inflight_observations(&self, work_item_id: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "DELETE FROM ci_inflight_observations WHERE work_item_id = ?1",
            params![work_item_id],
        )?;
        Ok(())
    }

    /// Count `ci_remediations` rows for `work_item_id` created within
    /// the last `window_secs` seconds. Used by the Phase 12 #40 churn
    /// guard: a manual `boss engine ci retry` invocation that pushes
    /// the count to ≥5 over the last hour signals the engine should
    /// rate-limit the next retry until the user explicitly overrides.
    pub fn count_recent_ci_remediations(
        &self,
        work_item_id: &str,
        window_secs: i64,
    ) -> Result<i64> {
        let conn = self.connect()?;
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let cutoff = now_secs.saturating_sub(window_secs);
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM ci_remediations
              WHERE work_item_id = ?1
                AND CAST(created_at AS INTEGER) >= ?2",
            params![work_item_id, cutoff],
            |row| row.get(0),
        )?;
        Ok(n)
    }

    /// Phase 12 #40 — churn guard for the `boss engine ci retry`
    /// verb. The retry path bumps `ci_attempts_used` back to 0 and
    /// re-fires the auto-fix flow; calling it repeatedly without
    /// addressing the underlying failure puts the chore in a tight
    /// retry loop. We rate-limit when the work item has ≥
    /// [`CI_CHURN_LIMIT`] `ci_remediations` rows created in the last
    /// hour. The verb implementation passes the user-supplied
    /// `--force` override through `allow_override`; with the override
    /// set, the function always returns `false` so the engine still
    /// fires the retry (but the caller is expected to surface a loud
    /// warning to the user before doing so).
    pub fn is_ci_retry_rate_limited(
        &self,
        work_item_id: &str,
        allow_override: bool,
    ) -> Result<bool> {
        if allow_override {
            return Ok(false);
        }
        let count = self.count_recent_ci_remediations(work_item_id, CI_CHURN_WINDOW_SECS)?;
        Ok(count >= CI_CHURN_LIMIT)
    }

    /// Flip a pending `ci_remediations` attempt to `succeeded` and
    /// stamp `head_sha_after` if known. Idempotent — a row already
    /// terminal returns `Ok(None)` and writes nothing.
    pub fn mark_ci_remediation_succeeded(
        &self,
        attempt_id: &str,
        head_sha_after: Option<&str>,
    ) -> Result<Option<CiRemediation>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE ci_remediations
                SET status         = 'succeeded',
                    head_sha_after = COALESCE(?2, head_sha_after),
                    finished_at    = COALESCE(finished_at, ?3)
              WHERE id = ?1
                AND status IN ('pending', 'running')",
            params![attempt_id, head_sha_after, now],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        let updated = query_ci_remediation(&tx, attempt_id)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Flip a `ci_remediations` attempt to `running` and stamp the
    /// coordinator-owned spawn metadata (lease, workspace, worker
    /// pane). Mirrors [`Self::mark_conflict_resolution_running`].
    /// Used by the Phase 9 spawn-flow wiring and by Phase 10 #33's
    /// completion finalizer tests. Idempotent over a row already in
    /// `running` — the WHERE guard accepts both `pending` and
    /// `running` so a re-spawn after engine restart re-stamps the
    /// columns without rejecting.
    pub fn mark_ci_remediation_running(
        &self,
        attempt_id: &str,
        cube_lease_id: &str,
        cube_workspace_id: &str,
        worker_id: &str,
    ) -> Result<Option<CiRemediation>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE ci_remediations
                SET status            = 'running',
                    cube_lease_id     = ?2,
                    cube_workspace_id = ?3,
                    worker_id         = ?4,
                    started_at        = COALESCE(started_at, ?5)
              WHERE id = ?1
                AND status IN ('pending', 'running')",
            params![attempt_id, cube_lease_id, cube_workspace_id, worker_id, now],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        let updated = query_ci_remediation(&tx, attempt_id)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Flip a non-terminal `ci_remediations` attempt to `failed` with
    /// `failure_reason`. Mirrors
    /// [`Self::mark_conflict_resolution_failed`]. Used by the
    /// completion-path catch-all (design Phase 10 #33) when a worker
    /// exits without pushing and without calling
    /// `boss engine ci mark-failed` to classify its own outcome —
    /// the engine defaults to `failure_reason='no_push_no_classification'`.
    /// Idempotent — a row already terminal returns `Ok(None)`.
    pub fn mark_ci_remediation_failed(
        &self,
        attempt_id: &str,
        reason: &str,
    ) -> Result<Option<CiRemediation>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE ci_remediations
                SET status         = 'failed',
                    failure_reason = ?2,
                    finished_at    = COALESCE(finished_at, ?3)
              WHERE id = ?1
                AND status IN ('pending', 'running')",
            params![attempt_id, reason, now],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        let updated = query_ci_remediation(&tx, attempt_id)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Engine-side abandon for a `ci_remediations` attempt. Used for
    /// the budget-exhausted / opt-out / suppression paths — the
    /// engine declined to spawn, so the attempt row never ran.
    pub fn mark_ci_remediation_abandoned(
        &self,
        attempt_id: &str,
        reason: &str,
    ) -> Result<Option<CiRemediation>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE ci_remediations
                SET status         = 'abandoned',
                    failure_reason = ?2,
                    finished_at    = COALESCE(finished_at, ?3)
              WHERE id = ?1
                AND status IN ('pending', 'running')",
            params![attempt_id, reason, now],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        let updated = query_ci_remediation(&tx, attempt_id)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Store the engine-collected log tail on a pending `ci_remediations`
    /// attempt. Mirrors [`Self::set_conflict_resolution_diagnosis`]; the
    /// coordinator calls this pre-spawn (Phase 9 #27) after running the
    /// per-provider `CiLogReader::read_log_tail`. Idempotent — overwriting
    /// is fine because the row is `pending` and the worker hasn't read
    /// it yet. `Ok(None)` when the id is missing.
    pub fn set_ci_remediation_log_excerpt(
        &self,
        attempt_id: &str,
        log_excerpt: &str,
    ) -> Result<Option<CiRemediation>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let rows = tx.execute(
            "UPDATE ci_remediations
                SET log_excerpt = ?2
              WHERE id = ?1",
            params![attempt_id, log_excerpt],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        let updated = query_ci_remediation(&tx, attempt_id)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Record the worker's post-log triage decision on a `running`
    /// attempt. Values per design §Q4: `'tractable'`, `'flaky_or_infra'`,
    /// `'unfixable'`. Pure metadata column — no state machine effect.
    /// Idempotent overwrite.
    pub fn set_ci_remediation_triage_class(
        &self,
        attempt_id: &str,
        triage_class: &str,
    ) -> Result<Option<CiRemediation>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let rows = tx.execute(
            "UPDATE ci_remediations
                SET triage_class = ?2
              WHERE id = ?1",
            params![attempt_id, triage_class],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        let updated = query_ci_remediation(&tx, attempt_id)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Mark a `fix`-kind attempt as a "succeeded via rebase only" run —
    /// the worker rebased onto base HEAD, force-pushed, and CI came back
    /// green without any code change. The reconciled-2026-05-17 layered
    /// design call (see project description) keeps this row out of the
    /// per-PR budget: the worker calls
    /// `boss engine ci mark-succeeded-via-rebase <attempt-id>` and the
    /// engine atomically (a) flips `status` to `succeeded`, (b) sets
    /// `consumes_budget = 0` so the post-cycle counter delta is zero,
    /// (c) stamps `failure_reason = 'rebase_only'` as the audit
    /// discriminator (a non-`NULL` value in this column on a
    /// `'succeeded'` row means "did not consume budget"), and (d)
    /// decrements `tasks.ci_attempts_used` by one to undo the
    /// detection-side bump (only when the row was originally
    /// `consumes_budget = 1`; idempotent re-call is a no-op).
    /// Idempotent — `Ok(None)` on a terminal row.
    pub fn mark_ci_remediation_succeeded_via_rebase(
        &self,
        attempt_id: &str,
    ) -> Result<Option<CiRemediation>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        // Snapshot the row so we know whether to decrement the counter.
        // A second call for the same id finds `status = 'succeeded'` and
        // the WHERE guard misses, so the counter is touched at most once.
        let snapshot = query_ci_remediation(&tx, attempt_id)?;
        let rows = tx.execute(
            "UPDATE ci_remediations
                SET status          = 'succeeded',
                    consumes_budget = 0,
                    failure_reason  = COALESCE(failure_reason, 'rebase_only'),
                    finished_at     = COALESCE(finished_at, ?2)
              WHERE id = ?1
                AND status IN ('pending', 'running')",
            params![attempt_id, now],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(None);
        }
        if let Some(snap) = snapshot {
            if snap.consumes_budget != 0 {
                tx.execute(
                    "UPDATE tasks
                        SET ci_attempts_used = CASE
                                WHEN ci_attempts_used > 0 THEN ci_attempts_used - 1
                                ELSE 0
                            END
                      WHERE id = ?1
                        AND deleted_at IS NULL",
                    params![snap.work_item_id],
                )?;
            }
        }
        let updated = query_ci_remediation(&tx, attempt_id)?;
        tx.commit()?;
        Ok(updated)
    }
}
