use super::*;

impl WorkDb {
    /// Returns or creates a ready execution for `work_item_id`, applying any
    /// priority / preferred-workspace overrides from the request.
    ///
    /// Friendly ids (`T3`, `P7`) are resolved to primary ids before any other
    /// processing, so callers do not need to pre-resolve them.
    ///
    /// If the most recent execution for this work item is still in flight
    /// (`ready` / `running` / `waiting_*`) we update its priority and
    /// preferred_workspace_id rather than creating a duplicate. If it is
    /// terminal (or absent), we insert a fresh `ready` execution.
    pub fn request_execution(&self, input: RequestExecutionInput) -> Result<WorkExecution> {
        // No live-worker oracle → assume every non-terminal execution
        // is genuinely live (the historical behaviour, kept for tests
        // that don't stand up the live registry).
        self.request_execution_with_live_check(input, |_| true)
    }

    /// Same as `request_execution`, but the caller supplies a
    /// predicate that says whether the execution id named by an
    /// existing non-terminal row corresponds to a worker that is
    /// **actually live** in the engine's slot registry. When the
    /// predicate returns `false` we treat the existing execution as
    /// stale (mark it `abandoned`, finished now) and create a fresh
    /// `ready` execution. This is what lets a kanban drag-to-Doing
    /// re-dispatch a chore whose previous worker died with the app
    /// before reaching `done`.
    ///
    /// Idempotency contract:
    /// - existing execution terminal or absent → insert new `ready`,
    /// - existing non-terminal AND predicate returns `true` → no-op
    ///   (just refresh priority / preferred_workspace_id, same as
    ///   before),
    /// - existing non-terminal AND predicate returns `false` → mark
    ///   existing `abandoned`, insert new `ready`.
    pub fn request_execution_with_live_check<F: FnOnce(&str) -> bool>(
        &self,
        mut input: RequestExecutionInput,
        is_live: F,
    ) -> Result<WorkExecution> {
        let mut conn = self.connect()?;
        // Resolve T42 / P7 friendly ids to primary ids before any other check,
        // so callers like `bossctl work start T3` work without client-side
        // resolution. Primary ids (task_*, proj_*, prod_*) pass through unchanged.
        if let Some(resolved) = resolve_friendly_work_item_id(&conn, &input.work_item_id)? {
            input.work_item_id = resolved;
        }
        ensure_dispatch_repo_resolvable(&mut conn, &input.work_item_id)?;
        let tx = conn.transaction()?;
        let execution = request_execution_in_tx_with_live_check(&tx, input, is_live)?;
        tx.commit()?;
        Ok(execution)
    }

    /// Repo-resolution precheck that does not create or mutate any
    /// `work_executions` row. The kanban drag-to-Doing path calls this
    /// before flipping `tasks.status = 'active'` so a deterministic
    /// dispatch failure (no product default repo, no per-task
    /// override) rejects the `UpdateWorkItem` instead of leaving the
    /// card stuck in Doing with no worker (bug #679). Shares the same
    /// error text and sticky attention item that the request-execution
    /// path writes, so the kanban Attention lane sees the same shape
    /// regardless of which trigger surfaced the problem.
    pub fn precheck_dispatch_repo(&self, work_item_id: &str) -> Result<()> {
        let mut conn = self.connect()?;
        let resolved = resolve_friendly_work_item_id(&conn, work_item_id)?
            .unwrap_or_else(|| work_item_id.to_owned());
        ensure_dispatch_repo_resolvable(&mut conn, &resolved)
    }

    /// Demote `tasks.status = 'active'` rows that never made it past
    /// dispatch — i.e., no `work_runs` row was ever recorded for any
    /// of the work item's executions — back to `todo`. Any non-terminal
    /// executions on those work items are stamped `abandoned` in the
    /// same transaction so the dispatcher won't pick them up after the
    /// demote.
    ///
    /// This is the boot-time "ghost active" sweep: a chore can land in
    /// `tasks.status = 'active'` without ever spawning a worker if the
    /// previous engine crashed between flipping the kanban status and
    /// claiming a slot, or if a `RequestExecution` raced ahead of the
    /// dispatcher and no slot was free. The Doing column should not
    /// show those — they have no run history and should fall back to
    /// the To-Do lane so the human can retry.
    ///
    /// Demotion also stamps `last_status_actor = 'engine'` so the
    /// kanban surface can distinguish the engine's auto-demote from a
    /// human drag, and returns the per-row `product_id` so the caller
    /// can publish a work-item-changed event on the product's topic —
    /// without that event the UI keeps showing the card in Doing
    /// until the next manual refetch.
    ///
    /// Returns one [`HealedGhostActive`] per demoted row. Items whose
    /// executions already produced a run (active worker that crashed,
    /// terminated cleanly, or is still executing) are left alone —
    /// `reconcile_active_dispatch` handles those via re-dispatch.
    pub fn heal_ghost_active_chores(&self) -> Result<Vec<HealedGhostActive>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let candidates: Vec<(String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT t.id, t.product_id FROM tasks t
                 WHERE t.status = 'active'
                   AND t.deleted_at IS NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM work_runs wr
                       JOIN work_executions we ON wr.execution_id = we.id
                       WHERE we.work_item_id = t.id
                   )",
            )?;
            stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut healed = Vec::new();
        let now = now_string();
        for (work_item_id, product_id) in candidates {
            // Abandon any non-terminal executions so they don't get
            // picked up by the dispatcher after the demote. Terminal
            // executions are left alone — they're already settled.
            tx.execute(
                "UPDATE work_executions
                 SET status = 'abandoned',
                     finished_at = COALESCE(finished_at, ?2)
                 WHERE work_item_id = ?1
                   AND status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')",
                params![work_item_id, now],
            )?;
            // Demote the kanban status. Use a guarded update so we
            // don't race a concurrent move to `done`/`archived`.
            // Stamps `last_status_actor = 'engine'` so the kanban can
            // render "demoted by engine: dispatch never reached a
            // worker" instead of attributing the move to the human who
            // last touched the row.
            let updated = tx.execute(
                "UPDATE tasks
                 SET status = 'todo',
                     last_status_actor = 'engine',
                     updated_at = ?2
                 WHERE id = ?1
                   AND status = 'active'
                   AND deleted_at IS NULL",
                params![work_item_id, now],
            )?;
            if updated > 0 {
                healed.push(HealedGhostActive {
                    work_item_id,
                    product_id,
                });
            }
        }
        tx.commit()?;
        Ok(healed)
    }

    /// Demote a single `active` work item back to `todo` after its
    /// dispatch failed before a worker ever came up (e.g. the worker
    /// pane could not be spawned because no app session was registered,
    /// libghostty IPC dropped, or the slot was busy). Without this the
    /// card is stranded in the Doing column behind a dead execution and
    /// the orphan-active sweep keeps re-dispatching the same doomed
    /// spawn every cycle. Demoting it surfaces the failure as a return
    /// to To-Do so the human can retry deliberately.
    ///
    /// Guarded on `status = 'active'` so a concurrent move to
    /// `done`/`archived`/`blocked` is never stomped. Stamps
    /// `last_status_actor = 'engine'` (same as `heal_ghost_active_chores`)
    /// so the kanban attributes the demote to the engine, not the human
    /// who last touched the row. Returns `true` if a row was demoted.
    pub fn demote_active_work_item_to_todo(&self, work_item_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let now = now_string();
        let updated = conn.execute(
            "UPDATE tasks
             SET status = 'todo',
                 last_status_actor = 'engine',
                 updated_at = ?2
             WHERE id = ?1
               AND status = 'active'
               AND deleted_at IS NULL",
            params![work_item_id, now],
        )?;
        Ok(updated > 0)
    }

    /// Re-issue `RequestExecution` for every non-deleted task / chore
    /// whose status is `active` but whose latest execution is terminal
    /// (or which has no execution). This is the engine-startup
    /// rehydration described in `work-kanban.md` §3 of the
    /// Doing-column dispatch contract: the kanban Doing column is
    /// supposed to mirror "running or queued," and after a crash the
    /// only remaining signal of "this was supposed to be running" is
    /// `tasks.status = 'active'`. Returns the work item ids that were
    /// re-dispatched so the caller can log them.
    ///
    /// `is_live` is the same predicate `request_execution_with_live_check`
    /// uses. Engine startup runs reconcile *before* any worker spawn
    /// could have happened, so the natural caller passes a closure that
    /// returns `false` for everything — every existing non-terminal
    /// execution is treated as stale and re-dispatched. Tests that
    /// don't stand up a live registry can pass `|_| true` to keep the
    /// pre-live-check semantics.
    pub fn reconcile_active_dispatch<F: Fn(&str) -> bool>(
        &self,
        is_live: F,
    ) -> Result<Vec<String>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        // Active, non-deleted task/chore rows are the candidate set.
        let candidate_ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM tasks
                 WHERE status = 'active' AND deleted_at IS NULL",
            )?;
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut redispatched = Vec::new();
        for work_item_id in candidate_ids {
            // Decide whether this work item needs a fresh ready
            // execution. The candidate cases are:
            //   - no execution at all → yes,
            //   - latest execution terminal → yes,
            //   - latest execution non-terminal but `is_live`
            //     reports the slot is gone → yes (stale row).
            let existing = query_latest_execution_for_work_item(&tx, &work_item_id)?;
            let needs_dispatch = match &existing {
                Some(existing) => {
                    execution_status_is_terminal(&existing.status) || !is_live(&existing.id)
                }
                None => true,
            };
            if !needs_dispatch {
                continue;
            }
            // When the predecessor was orphaned by the startup reaper
            // (worker pane died across the engine restart), default
            // the new ready row's `preferred_workspace_id` to the
            // orphan's `cube_workspace_id`. The orphan's workspace
            // typically still holds in-flight commits the human wants
            // resumed — without this hint the dispatcher would lease
            // any free workspace and the fresh worker would start
            // against `main` on an unrelated branch. Only fires for
            // orphaned predecessors; abandoned / failed / cancelled
            // ones are intentional throwaways and don't carry forward.
            let preferred_workspace_id = existing
                .as_ref()
                .filter(|prev| prev.status == "orphaned")
                .and_then(|prev| prev.cube_workspace_id.clone());
            request_execution_in_tx_with_live_check(
                &tx,
                RequestExecutionInput {
                    work_item_id: work_item_id.clone(),
                    priority: None,
                    preferred_workspace_id,
                    force: false,
                },
                |run_id| is_live(run_id),
            )?;
            redispatched.push(work_item_id);
        }
        tx.commit()?;
        Ok(redispatched)
    }

    /// Steady-state counterpart of [`Self::reconcile_active_dispatch`]
    /// used by the dispatcher when a worker frees up. Re-issues
    /// `RequestExecution` for every active task/chore whose latest
    /// execution is missing or terminal — i.e., the items the
    /// create-time dispatch couldn't place because the pool was full
    /// or whose worker died after the kanban moved them to `active`.
    ///
    /// Differs from `reconcile_active_dispatch` in three ways:
    ///
    /// 1. Honours the per-task `autostart` flag. Items with
    ///    `autostart=false` are deliberately parked in `active` until
    ///    a human resumes them — the on-free rescan must not
    ///    auto-restart them silently. The startup reconcile rehydrates
    ///    them once because everything is being brought back online,
    ///    but a recurring rescan would loop on a chore that died for
    ///    a reason the user already opted out of auto-handling.
    /// 2. Skips items that are dependency-gated (a `blocks` prereq is
    ///    still unmet) instead of bailing the whole transaction.
    /// 3. Orders the candidate set by `tasks.updated_at ASC` so the
    ///    rescan acts FIFO — the chore that has been waiting longest
    ///    gets the freed worker first.
    ///
    /// Items whose latest execution is still non-terminal (`ready`,
    /// `running`, `waiting_*`) are left alone — `kick()` already
    /// consumes the `ready` queue, and the others are owned by a
    /// live worker or the dependency engine. Returns the work item
    /// ids that were freshly redispatched so the caller can log them.
    pub fn rescan_active_dispatch(&self) -> Result<Vec<String>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        // FIFO by `updated_at` so the chore that has been waiting
        // longest gets the freed worker. `id` is the deterministic
        // tie-breaker for rows that share an updated_at second.
        let candidates: Vec<(String, bool)> = {
            let mut stmt = tx.prepare(
                "SELECT id, autostart FROM tasks
                 WHERE status = 'active' AND deleted_at IS NULL
                 ORDER BY updated_at ASC, id ASC",
            )?;
            stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut redispatched = Vec::new();
        for (work_item_id, autostart) in candidates {
            if !autostart {
                continue;
            }
            let needs_dispatch = match query_latest_execution_for_work_item(&tx, &work_item_id)? {
                Some(existing) => execution_status_is_terminal(&existing.status),
                None => true,
            };
            if !needs_dispatch {
                continue;
            }
            // Silently skip gated items so the rescan keeps going.
            // request_execution_in_tx_with_live_check would bail and
            // roll back the entire transaction otherwise.
            if !deps::gating_prereqs_for(&tx, &work_item_id)?.is_empty() {
                continue;
            }
            request_execution_in_tx_with_live_check(
                &tx,
                RequestExecutionInput {
                    work_item_id: work_item_id.clone(),
                    priority: None,
                    preferred_workspace_id: None,
                    force: false,
                },
                // `|_| true` keeps any non-terminal execution intact —
                // the on-free rescan only ever fires this branch when
                // the latest execution is terminal anyway, so the
                // closure is unreachable in the redispatch path.
                |_| true,
            )?;
            redispatched.push(work_item_id);
        }
        tx.commit()?;
        Ok(redispatched)
    }

    /// Return the work item ids whose `tasks.status = 'active'` but
    /// whose latest execution is NOT in `running` (no live worker is
    /// currently driving the slot). Used by the dispatcher to surface
    /// the "active vs slot" invariant when the worker pool stalls so a
    /// human reviewing the engine log can spot a divergence between
    /// `boss chore list --status active` and `bossctl agents list`.
    ///
    /// Items whose latest execution is `ready` (queued behind a full
    /// pool) are included — they're the canonical "queued ghost" the
    /// invariant is meant to catch.
    pub fn list_active_chores_without_live_run(&self) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT t.id FROM tasks t
             WHERE t.status = 'active'
               AND t.deleted_at IS NULL
               AND NOT EXISTS (
                   SELECT 1 FROM work_executions we
                   WHERE we.work_item_id = t.id
                     AND we.status = 'running'
               )",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Return the work item ids that are candidates for orphan-active
    /// redispatch. A candidate satisfies all of:
    ///
    /// 1. `tasks.status = 'active'` and not deleted.
    /// 2. `tasks.updated_at` is more than `min_age_secs` old (guards
    ///    against false-positive on a fresh transition whose worker is
    ///    still spinning up).
    /// 3. No `ready` execution exists (if one does, it is already
    ///    queued for dispatch; no action needed).
    ///
    /// The caller is responsible for checking whether the latest
    /// non-terminal execution (if any) is claimed by a live worker
    /// slot — that check requires in-memory worker-pool state that the
    /// DB layer does not have access to.
    pub fn list_orphan_active_candidates(&self, min_age_secs: i64) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let now_secs: i64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let cutoff = now_secs - min_age_secs;
        // The recovery-escalation exclusion: once the transient-recovery
        // sweep has raised an open attention item because a worker's API
        // error is non-retryable (permanent/unrecognised) or the retry
        // cap was reached, this work item must NOT be blindly
        // re-dispatched — it is flagged for a human. Resolving the
        // attention item makes it a candidate again.
        let stmt_sql = format!(
            "SELECT t.id FROM tasks t
             WHERE t.status = 'active'
               AND t.deleted_at IS NULL
               AND CAST(t.updated_at AS INTEGER) < ?1
               AND NOT EXISTS (
                   SELECT 1 FROM work_executions we
                   WHERE we.work_item_id = t.id
                     AND we.status = 'ready'
               )
               AND NOT EXISTS (
                   SELECT 1 FROM work_attention_items a
                   WHERE a.work_item_id = t.id
                     AND a.status = 'open'
                     AND a.kind IN ('{permanent}', '{exhausted}')
               )
             ORDER BY t.updated_at ASC, t.id ASC",
            permanent = ATTENTION_KIND_RECOVERY_PERMANENT,
            exhausted = ATTENTION_KIND_RECOVERY_EXHAUSTED,
        );
        let mut stmt = conn.prepare(&stmt_sql)?;
        let rows = stmt.query_map([cutoff], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Count how many terminal executions (`orphaned`, `abandoned`,
    /// `failed`) the work item has produced within the trailing
    /// `since_epoch_secs` window. Used by the orphan-active churn
    /// guard to stop auto-redispatching a work item that keeps dying.
    pub fn count_recent_terminal_executions(
        &self,
        work_item_id: &str,
        since_epoch_secs: i64,
    ) -> Result<i64> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT COUNT(*) FROM work_executions
              WHERE work_item_id = ?1
                AND status IN ('orphaned', 'abandoned', 'failed')
                AND CAST(created_at AS INTEGER) >= ?2",
            params![work_item_id, since_epoch_secs],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    pub fn list_executions(&self, work_item_id: Option<&str>) -> Result<Vec<WorkExecution>> {
        let conn = self.connect()?;
        if let Some(work_item_id) = work_item_id {
            let _ = product_id_for_work_item(&conn, work_item_id)?;
            let mut stmt = conn.prepare(
                "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                        cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                        created_at, started_at, finished_at,
                        pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count
                 FROM work_executions
                 WHERE work_item_id = ?1
                 ORDER BY created_at ASC, id ASC",
            )?;
            let rows = stmt.query_map([work_item_id], map_execution)?;
            return collect_rows(rows);
        }

        let mut stmt = conn.prepare(
            "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                    cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                    created_at, started_at, finished_at,
                    pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count
             FROM work_executions
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], map_execution)?;
        collect_rows(rows)
    }

    pub fn get_execution(&self, id: &str) -> Result<WorkExecution> {
        let conn = self.connect()?;
        query_execution(&conn, id)?.with_context(|| format!("unknown execution: {id}"))
    }

    /// Find the most recent `orphaned` execution for a work item that has
    /// no `pr_url` set. Used by the runner at spawn time to detect a
    /// prior mid-flight execution whose branch the new worker should
    /// attempt to resume (startup recovery path).
    ///
    /// Returns `None` when:
    ///   - the work item has no prior executions,
    ///   - all prior executions are non-orphaned (completed, failed, etc.), or
    ///   - the latest orphaned execution already has `pr_url` set (that
    ///     case is handled by the existing `task.pr_url` resume path).
    ///
    /// The `current_execution_id` is excluded so the caller doesn't
    /// accidentally match the execution that's currently being dispatched.
    pub fn get_prior_orphaned_execution(
        &self,
        work_item_id: &str,
        current_execution_id: &str,
    ) -> Result<Option<WorkExecution>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                    cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                    created_at, started_at, finished_at,
                    pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count
             FROM work_executions
             WHERE work_item_id = ?1
               AND id != ?2
               AND status = 'orphaned'
               AND pr_url IS NULL
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
            rusqlite::params![work_item_id, current_execution_id],
            map_execution,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Return the most recent `running` or `waiting_human` execution for
    /// `work_item_id`, excluding `exclude_id`. Used by the double-spawn
    /// guard in the coordinator: before spawning, if another execution is
    /// already live, the new one is redundant and should be abandoned
    /// without starting a worker.
    pub fn get_live_execution_for_work_item(
        &self,
        work_item_id: &str,
        exclude_id: &str,
    ) -> Result<Option<WorkExecution>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                    cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                    created_at, started_at, finished_at,
                    pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count
             FROM work_executions
             WHERE work_item_id = ?1
               AND id != ?2
               AND status IN ('running', 'waiting_human')
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
            rusqlite::params![work_item_id, exclude_id],
            map_execution,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Mark an execution `abandoned` without touching any other
    /// execution or task state. Used by the double-spawn guard to
    /// discard a redundant `ready` execution before it ever reaches
    /// `start_execution_run`.
    pub fn mark_execution_redundant(&self, execution_id: &str) -> Result<()> {
        let conn = self.connect()?;
        let now = now_string();
        conn.execute(
            "UPDATE work_executions
             SET status = 'abandoned',
                 finished_at = COALESCE(finished_at, ?2)
             WHERE id = ?1",
            rusqlite::params![execution_id, now],
        )?;
        Ok(())
    }
}
