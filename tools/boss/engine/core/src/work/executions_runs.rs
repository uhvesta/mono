use super::*;

impl WorkDb {
    /// Mark an execution `cancelled` and stamp `finished_at`. Errors
    /// when the execution is unknown or already in a terminal status
    /// — callers shouldn't try to cancel a row that's already done.
    ///
    /// If the backing work item is currently `active` (the kanban
    /// Doing column), it's reset to `todo` so the card returns to the
    /// To-Do lane. `in_review`, `done`, and `archived` are preserved:
    /// `in_review` means a PR exists and cancel doesn't retract that
    /// PR, and `done`/`archived` are explicit human transitions that
    /// the auto-dispatch path is forbidden from downgrading.
    ///
    /// Workspace lease columns are intentionally left intact so the
    /// caller can hand the execution id to
    /// `WorkerCompletionHandler::force_release`, which transfers
    /// lease ownership atomically by clearing the columns itself
    /// before talking to the cube CLI. Trying to clear them inside
    /// this transaction would race the same release path.
    pub fn cancel_execution(&self, execution_id: &str) -> Result<WorkExecution> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let existing = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if execution_status_is_terminal(&existing.status) {
            bail!(
                "execution {execution_id} is already in terminal status `{}` and cannot be cancelled",
                existing.status
            );
        }
        let now = now_string();
        tx.execute(
            "UPDATE work_executions
             SET status = 'cancelled',
                 finished_at = ?2
             WHERE id = ?1",
            params![execution_id, now.as_str()],
        )?;
        // Move the kanban card back to To-Do for tasks/chores that
        // were `active` (Doing). Scoped to `active` only so we don't
        // clobber a `done`/`archived`/`in_review` transition.
        tx.execute(
            "UPDATE tasks
             SET status = 'todo',
                 updated_at = ?2
             WHERE id = ?1
               AND deleted_at IS NULL
               AND status = 'active'",
            params![existing.work_item_id, now.as_str()],
        )?;
        let updated = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution after cancel: {execution_id}"))?;
        tx.commit()?;
        Ok(updated)
    }

    /// Transition a non-terminal execution to the `orphaned` terminal
    /// status. Used by the startup reaper and the manual `bossctl
    /// agents reap` path when a worker process has died (or is
    /// presumed dead) but the engine has no other clean signal that
    /// it should stop treating the row as live.
    ///
    /// The workspace lease columns (`cube_lease_id`,
    /// `cube_workspace_id`, `workspace_path`) are intentionally left
    /// intact. The brief is explicit: do NOT release the cube
    /// workspace lease here — the workspace may still have in-flight
    /// commits from the dead worker that a fresh execution should
    /// resume against. Lease cleanup is a separate concern (cube TTL
    /// expiry or explicit `bossctl agents stop`).
    ///
    /// Any non-terminal `work_runs` rows attached to the execution are
    /// stamped `orphaned` with the same reason recorded as
    /// `result_summary`, so the run history reflects how the row went
    /// terminal rather than leaving it `active` forever.
    ///
    /// Errors when the execution is unknown or already terminal —
    /// callers shouldn't try to reap a row that's already done.
    pub fn mark_execution_orphaned(
        &self,
        execution_id: &str,
        reason: &str,
    ) -> Result<WorkExecution> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let existing = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if execution_status_is_terminal(&existing.status) {
            bail!(
                "execution {execution_id} is already in terminal status `{}` and cannot be reaped as orphaned",
                existing.status
            );
        }
        let now = now_string();
        tx.execute(
            "UPDATE work_executions
             SET status = 'orphaned',
                 finished_at = COALESCE(finished_at, ?2)
             WHERE id = ?1",
            params![execution_id, now.as_str()],
        )?;
        // Stamp any still-active work_runs as orphaned so the run
        // history matches the execution status. result_summary holds
        // the reaper's reason so an operator inspecting the row can
        // see why the engine terminated it.
        tx.execute(
            "UPDATE work_runs
             SET status = 'orphaned',
                 result_summary = COALESCE(result_summary, ?3),
                 finished_at = COALESCE(finished_at, ?2)
             WHERE execution_id = ?1
               AND finished_at IS NULL",
            params![execution_id, now.as_str(), reason],
        )?;
        let updated = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution after orphan reap: {execution_id}"))?;
        tx.commit()?;
        Ok(updated)
    }

    /// Auto-resume a work item whose worker stalled or died on a
    /// *transient* Claude API error. In one transaction:
    ///
    ///   1. If `dead_execution_id` is still non-terminal, mark it
    ///      `orphaned` (and stamp any still-active runs `orphaned` with
    ///      `reason`). Orphaned — not abandoned — so the runner's
    ///      startup-recovery path ([`Self::get_prior_orphaned_execution`])
    ///      finds it and directs the new worker to resume the prior
    ///      branch instead of starting from `main`.
    ///   2. Insert a fresh `ready` execution for the same work item that
    ///      **prefers the same cube workspace** (so cube's `--prefer`
    ///      re-leases it and in-progress work in the jj workspace is not
    ///      lost), carries `transient_failure_count = new_count`, and is
    ///      deferred until `dispatch_not_before_epoch` (the backoff
    ///      window — same `dispatch_not_before` gate the pre-start retry
    ///      path uses, honoured by [`Self::list_ready_executions`]).
    ///
    /// Returns the new `ready` execution. The caller releases the worker
    /// pool slot and emits the dispatch event. Because the work item now
    /// has a `ready` execution, the orphan-active sweep skips it (no
    /// double dispatch).
    pub fn request_resume_execution(
        &self,
        dead_execution_id: &str,
        new_count: i64,
        dispatch_not_before_epoch: i64,
        reason: &str,
    ) -> Result<WorkExecution> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let dead = query_execution(&tx, dead_execution_id).require("execution", dead_execution_id)?;

        let now = now_string();
        if !execution_status_is_terminal(&dead.status) {
            tx.execute(
                "UPDATE work_executions
                 SET status = 'orphaned',
                     finished_at = COALESCE(finished_at, ?2)
                 WHERE id = ?1",
                params![dead_execution_id, now.as_str()],
            )?;
            tx.execute(
                "UPDATE work_runs
                 SET status = 'orphaned',
                     result_summary = COALESCE(result_summary, ?3),
                     finished_at = COALESCE(finished_at, ?2)
                 WHERE execution_id = ?1
                   AND finished_at IS NULL",
                params![dead_execution_id, now.as_str(), reason],
            )?;
        }

        // Prefer the workspace the dead worker was actually leased into;
        // fall back to its recorded preference if the lease metadata was
        // never stamped. Hard prefer (prefer_is_soft carried from the
        // dead row) so the resume lands on the same jj checkout.
        let preferred_workspace_id = dead
            .cube_workspace_id
            .clone()
            .or_else(|| dead.preferred_workspace_id.clone());

        let new_id = next_id("exec");
        let dispatch_not_before = dispatch_not_before_epoch.to_string();
        let branch_naming_json =
            serde_json::to_string(&dead.branch_naming).unwrap_or_default();
        tx.execute(
            "INSERT INTO work_executions (
                id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                created_at, started_at, finished_at, prefer_is_soft,
                transient_failure_count, dispatch_not_before, allow_dirty, branch_naming
             ) VALUES (?1, ?2, ?3, 'ready', ?4, ?5, NULL, NULL, NULL, ?6, ?7, ?8, NULL, NULL, ?9, ?10, ?11, ?12, ?13)",
            params![
                new_id,
                dead.work_item_id,
                dead.kind.as_str(),
                dead.repo_remote_url,
                dead.cube_repo_id,
                dead.priority,
                preferred_workspace_id,
                now,
                dead.prefer_is_soft as i64,
                new_count,
                dispatch_not_before,
                dead.allow_dirty as i64,
                branch_naming_json,
            ],
        )?;

        let new_execution = query_execution(&tx, &new_id)?
            .with_context(|| format!("missing execution after resume insert: {new_id}"))?;
        tx.commit()?;
        Ok(new_execution)
    }

    /// Path of the most recent run's transcript for `execution_id`, or
    /// `None` if no run recorded one. Used by the transient-recovery
    /// sweep to read the worker's transcript tail (the ground-truth
    /// signal for whether it stalled on an API error).
    pub fn latest_transcript_path(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT transcript_path FROM work_runs
             WHERE execution_id = ?1 AND transcript_path IS NOT NULL
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
            params![execution_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map(Option::flatten)
        .map_err(Into::into)
    }

    /// Raise a work-item-scoped attention item for `work_item_id` unless
    /// one with the same `kind` is already open. Idempotent so repeated
    /// recovery-sweep passes don't pile up duplicate rows. Returns the
    /// existing or newly-created item's id. Used by the transient-recovery
    /// sweep to escalate non-retryable / retry-exhausted workers.
    pub fn upsert_work_item_attention(
        &self,
        work_item_id: &str,
        kind: &str,
        title: &str,
        body_markdown: &str,
    ) -> Result<String> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let _ = product_id_for_work_item(&tx, work_item_id)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT id FROM work_attention_items
                 WHERE work_item_id = ?1 AND kind = ?2 AND status = 'open'
                 ORDER BY created_at ASC, id ASC
                 LIMIT 1",
                params![work_item_id, kind],
                |row| row.get(0),
            )
            .optional()?;
        let id = match existing {
            Some(id) => id,
            None => {
                let id = next_id("attn");
                let now = now_string();
                tx.execute(
                    "INSERT INTO work_attention_items (
                        id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at
                     ) VALUES (?1, NULL, ?2, ?3, 'open', ?4, ?5, ?6, NULL)",
                    params![id, work_item_id, kind, title, body_markdown, now],
                )?;
                id
            }
        };
        tx.commit()?;
        Ok(id)
    }

    /// Return the run ids that belong to `execution_id` and have not
    /// yet finished. The cancel-execution flow uses this to find any
    /// libghostty pane the execution still backs so the engine can
    /// release it in addition to the cube workspace.
    pub fn active_run_ids_for_execution(&self, execution_id: &str) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id FROM work_runs
             WHERE execution_id = ?1
               AND finished_at IS NULL",
        )?;
        let rows = stmt.query_map([execution_id], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Build a map from `cube_lease_id` → `execution_id` for every
    /// execution row that currently records a lease. Used by
    /// `WorkspacePoolSummary` to annotate cube's view of the pool with
    /// the engine's own knowledge of which lease is backing which
    /// execution. Rows without a lease (`cube_lease_id IS NULL`) are
    /// skipped.
    pub fn lease_to_execution_map(&self) -> Result<HashMap<String, String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT cube_lease_id, id
             FROM work_executions
             WHERE cube_lease_id IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (lease_id, execution_id) = row?;
            map.insert(lease_id, execution_id);
        }
        Ok(map)
    }

    pub fn list_ready_executions(&self) -> Result<Vec<WorkExecution>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                    cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                    created_at, started_at, finished_at,
                    pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count, allow_dirty, branch_naming
             FROM work_executions
             WHERE status = 'ready'
               AND (dispatch_not_before IS NULL
                    OR CAST(dispatch_not_before AS INTEGER) <= CAST(strftime('%s', 'now') AS INTEGER))
             ORDER BY priority DESC, created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], map_execution)?;
        collect_rows(rows)
    }

    /// Return every `work_executions` row the engine considers "in
    /// flight": status is non-terminal AND a cube workspace lease was
    /// recorded against it (`cube_lease_id IS NOT NULL`). The startup
    /// reconciler probes these against cube state to decide whether
    /// the underlying worker is still alive — without that probe, the
    /// existing `reconcile_active_dispatch` redispatches every
    /// non-terminal row blindly because the live-worker registry is
    /// empty at boot, which is the bug that produced the duplicate
    /// dispatch on 2026-05-07.
    pub fn list_in_flight_executions(&self) -> Result<Vec<WorkExecution>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                    cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                    created_at, started_at, finished_at,
                    pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count, allow_dirty, branch_naming
             FROM work_executions
             WHERE status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
               AND cube_lease_id IS NOT NULL
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], map_execution)?;
        collect_rows(rows)
    }

    /// Return every non-terminal `revision_implementation` execution whose
    /// task is a revision in the chain rooted at `chain_root_id`.  Used by
    /// the merge poller to find in-flight revision workers to stop after
    /// the parent PR merges.  Only executions that hold a cube workspace
    /// lease are returned (same predicate as `list_in_flight_executions`).
    pub fn list_active_revision_executions_for_chain(
        &self,
        chain_root_id: &str,
    ) -> Result<Vec<WorkExecution>> {
        let conn = self.connect()?;
        let revision_ids = collect_chain_revision_ids(&conn, chain_root_id)?;
        if revision_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut executions = Vec::new();
        for rev_id in &revision_ids {
            let mut stmt = conn.prepare_cached(
                "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                        cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                        created_at, started_at, finished_at,
                        pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count, allow_dirty, branch_naming
                 FROM work_executions
                 WHERE work_item_id = ?1
                   AND kind = 'revision_implementation'
                   AND status NOT IN ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
                   AND cube_lease_id IS NOT NULL
                 ORDER BY created_at ASC, id ASC",
            )?;
            let rows = stmt.query_map([rev_id], map_execution)?;
            collect_rows(rows).map(|mut v| executions.append(&mut v))?;
        }
        Ok(executions)
    }

    pub fn reconcile_product_executions(
        &self,
        product_id: &str,
    ) -> Result<ExecutionReconcileResult> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let _product = query_product(&tx, product_id).require("product", product_id)?;
        let _projects = list_projects_for_product(&tx, product_id)?;
        let tasks = list_tasks_for_product(&tx, product_id)?;
        let mut result = ExecutionReconcileResult::default();

        // Per-row repo resolution lives inside
        // `reconcile_work_item_execution` now — the product default
        // is one of several fallbacks the resolver applies, not the
        // sole signal threaded through here.

        // Bucket the product's project-bound tasks by parent. Both
        // `kind = 'design'` and `kind = 'project_task'` share the
        // same first-incomplete-is-`ready` chain — design tasks live
        // at `ordinal = 0` so they sort to the head of the list and
        // dispatch first. The execution kind diverges per-row:
        // design dispatches as `project_design`, project_tasks as
        // `task_implementation`. This is the single point where the
        // project_design lifecycle plugs into the existing per-task
        // dispatch machinery; once routed the rest of the lifecycle
        // (PR detection, in_review→done, dependency cascade) is the
        // unchanged task path.
        let mut project_tasks: HashMap<String, Vec<Task>> = HashMap::new();
        for task in tasks {
            match task.kind {
                TaskKind::Chore => {
                    if task_accepts_execution(&task) {
                        reconcile_work_item_execution(
                            &tx,
                            &mut result,
                            &task.id,
                            ExecutionKind::ChoreImplementation,
                            "ready",
                        )?;
                    }
                }
                // Investigation tasks dispatch independently (no project
                // dependency chain) — each produces one standalone doc PR.
                TaskKind::Investigation => {
                    if task_accepts_execution(&task) {
                        reconcile_work_item_execution(
                            &tx,
                            &mut result,
                            &task.id,
                            ExecutionKind::InvestigationImplementation,
                            "ready",
                        )?;
                    }
                }
                // Revision tasks dispatch independently like investigations.
                // Each pushes a new commit to the *parent's* existing PR
                // branch rather than opening a new PR.  The gate checks
                // the chain root's status first (cached): if the parent PR
                // has already merged (chain root is `done`), the revision
                // is auto-blocked here rather than dispatched.
                TaskKind::Revision => {
                    if task_accepts_execution(&task) {
                        reconcile_revision_execution(&tx, &mut result, &task)?;
                    }
                }
                TaskKind::ProjectTask | TaskKind::Design => {
                    if let Some(project_id) = &task.project_id {
                        project_tasks
                            .entry(project_id.clone())
                            .or_default()
                            .push(task);
                    }
                }
                TaskKind::Task => {
                    // Plain task: no standalone execution; must be in a project.
                }
            }
        }

        for tasks in project_tasks.values_mut() {
            tasks.sort_by(|left, right| {
                left.ordinal
                    .unwrap_or(i64::MAX)
                    .cmp(&right.ordinal.unwrap_or(i64::MAX))
                    .then_with(|| left.created_at.cmp(&right.created_at))
                    .then_with(|| left.id.cmp(&right.id))
            });

            let first_incomplete = tasks.iter().position(task_accepts_execution);

            for (index, task) in tasks.iter().enumerate() {
                if !task_accepts_execution(task) {
                    continue;
                }
                let desired_status = if Some(index) == first_incomplete {
                    "ready"
                } else {
                    "waiting_dependency"
                };
                let execution_kind = match task.kind {
                    TaskKind::Design => ExecutionKind::ProjectDesign,
                    // All remaining kinds in this bucket are project_task rows;
                    // the other variants are handled before being bucketed here.
                    TaskKind::ProjectTask
                    | TaskKind::Chore
                    | TaskKind::Investigation
                    | TaskKind::Revision
                    | TaskKind::Task => ExecutionKind::TaskImplementation,
                };
                reconcile_work_item_execution(
                    &tx,
                    &mut result,
                    &task.id,
                    execution_kind,
                    desired_status,
                )?;
            }
        }

        tx.commit()?;
        Ok(result)
    }

    pub fn start_execution_run(
        &self,
        execution_id: &str,
        agent_id: &str,
        cube_repo_id: &str,
        cube_lease_id: &str,
        cube_workspace_id: &str,
        workspace_path: &str,
    ) -> Result<(WorkExecution, WorkRun)> {
        // Default to the local host. The distributed-execution dispatch
        // path (`schedule_execution`) calls `start_execution_run_on_host`
        // with the host the scheduler picked; every other caller is a
        // local-only run and inherits the `'local'` default.
        self.start_execution_run_on_host(
            execution_id,
            agent_id,
            cube_repo_id,
            cube_lease_id,
            cube_workspace_id,
            workspace_path,
            "local",
        )
    }

    /// Host-aware variant of [`Self::start_execution_run`]. Persists the
    /// scheduler-selected `host_id` onto both the new `work_runs` row and
    /// the `work_executions` row (per the distributed-execution design's
    /// "Storage Additions": the execution's `host_id` is "populated when a
    /// run first picks a host"; `work_runs.host_id` is the durable
    /// per-run attribution). `host_id = "local"` reproduces the historical
    /// behaviour exactly.
    #[allow(clippy::too_many_arguments)]
    pub fn start_execution_run_on_host(
        &self,
        execution_id: &str,
        agent_id: &str,
        cube_repo_id: &str,
        cube_lease_id: &str,
        cube_workspace_id: &str,
        workspace_path: &str,
        host_id: &str,
    ) -> Result<(WorkExecution, WorkRun)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if execution.status != "ready" {
            bail!(
                "execution {execution_id} is not ready and cannot start a run from status `{}`",
                execution.status
            );
        }

        let now = now_string();
        tx.execute(
            "UPDATE work_executions
             SET status = 'running',
                 cube_repo_id = ?2,
                 cube_lease_id = ?3,
                 cube_workspace_id = ?4,
                 workspace_path = ?5,
                 host_id = ?7,
                 started_at = COALESCE(started_at, ?6),
                 finished_at = NULL
             WHERE id = ?1",
            params![
                execution_id,
                cube_repo_id,
                cube_lease_id,
                cube_workspace_id,
                workspace_path,
                now,
                host_id
            ],
        )?;

        // Auto-advance the work item's kanban status to `active` so
        // the card moves into the Doing column when work begins.
        // Only applies to tasks/chores; products and projects use a
        // different status vocabulary and aren't rendered on the
        // kanban. Don't downgrade items already in `done` or
        // `archived` — manual transitions win.
        //
        // `in_review` is also protected: a row in Review owns an open
        // PR, and the only legitimate follow-up work on that PR is a
        // `kind=revision` task (a separate row that rides the base's
        // PR). A worker-start that lands on the base while it is
        // `in_review` — e.g. a stray re-dispatch / late execution
        // racing a revision's push — must NOT yank the base back out of
        // Review into Doing. Only an explicit human or merge action
        // advances a row out of Review. This guards the base row
        // (chore AND project_task — same machinery, same rule) for the
        // whole revision lifecycle. `reconcile_revision_execution` has
        // its own settle path that also abandons such stray executions
        // for engine-spawned *revision* rows; this guard is the
        // single, kind-agnostic backstop that closes the hole at the
        // source so the base never strands in Doing (revision-tasks
        // stranding regression).
        //
        // `autostart` is cleared here (single-shot semantics): once a
        // row has ever transitioned to Doing, the flag is consumed so
        // that moving the card back to Backlog later does not trigger
        // re-dispatch by the reconciler or orphan-active sweep.
        tx.execute(
            "UPDATE tasks
             SET status = 'active',
                 autostart = 0,
                 updated_at = ?2
             WHERE id = ?1
               AND deleted_at IS NULL
               AND status NOT IN ('done', 'archived', 'blocked', 'in_review')",
            params![execution.work_item_id, now],
        )?;

        let run_id = next_id("run");
        tx.execute(
            "INSERT INTO work_runs (
                id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                artifacts_path, created_at, started_at, finished_at, host_id
             ) VALUES (?1, ?2, ?3, 'active', NULL, NULL, NULL, NULL, ?4, ?4, NULL, ?5)",
            params![run_id, execution_id, agent_id, now, host_id],
        )?;

        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        let run = query_run(&tx, &run_id)?
            .with_context(|| format!("missing run after insert: {run_id}"))?;
        tx.commit()?;
        Ok((execution, run))
    }

    /// Record the head SHA of the chore's bound PR captured at run
    /// start. Used by the Stop-boundary SHA-delta gate to decide
    /// whether a resume run actually contributed to the bound PR
    /// before falling through to the `PROBE_NO_PR` nudge. Idempotent;
    /// callers may invoke once per execution start (or skip when no
    /// PR is bound). Empty `sha` is rejected — pass `None` semantics
    /// by simply not calling.
    pub fn set_execution_pr_head_before(&self, execution_id: &str, sha: &str) -> Result<()> {
        if sha.is_empty() {
            bail!("set_execution_pr_head_before: sha must be non-empty");
        }
        let conn = self.connect()?;
        let affected = conn.execute(
            "UPDATE work_executions SET pr_head_before = ?2 WHERE id = ?1",
            params![execution_id, sha],
        )?;
        if affected == 0 {
            bail!("unknown execution: {execution_id}");
        }
        Ok(())
    }

    pub fn fail_execution_start(
        &self,
        execution_id: &str,
        agent_id: &str,
        cube_repo_id: Option<&str>,
        error_text: &str,
    ) -> Result<(WorkExecution, WorkRun)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if execution.status != "ready" {
            bail!(
                "execution {execution_id} is not ready and cannot fail startup from status `{}`",
                execution.status
            );
        }

        let now = now_string();
        tx.execute(
            "UPDATE work_executions
             SET status = 'failed',
                 cube_repo_id = COALESCE(?2, cube_repo_id),
                 cube_lease_id = NULL,
                 cube_workspace_id = NULL,
                 workspace_path = NULL,
                 started_at = COALESCE(started_at, ?3),
                 finished_at = ?3
             WHERE id = ?1",
            params![execution_id, cube_repo_id, now],
        )?;

        let run_id = next_id("run");
        tx.execute(
            "INSERT INTO work_runs (
                id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                artifacts_path, created_at, started_at, finished_at
             ) VALUES (?1, ?2, ?3, 'failed', ?4, NULL, NULL, NULL, ?5, ?5, ?5)",
            params![run_id, execution_id, agent_id, error_text, now],
        )?;

        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        let run = query_run(&tx, &run_id)?
            .with_context(|| format!("missing run after insert: {run_id}"))?;
        tx.commit()?;
        Ok((execution, run))
    }

    /// Record a pre-start failure for `execution_id`, inserting a failed
    /// `work_run` and either resetting the execution to `ready` with a
    /// backoff delay (retry) or marking it permanently `failed`.
    ///
    /// `retry_delays` controls how many retries are allowed and the delay
    /// between each. An empty slice means "no retries; fail immediately."
    /// The Nth element is the backoff before the (N+1)th attempt.
    ///
    /// This is the safe-to-retry alternative to `fail_execution_start`:
    /// call it for failures at `cube_repo_ensure`, `workspace_lease`,
    /// `change_create`, and `run_start` (before the worker has any
    /// side effects). Do NOT call it for failures at or after
    /// `run_started` — those require `finish_execution_run`.
    pub fn record_pre_start_failure(
        &self,
        execution_id: &str,
        agent_id: &str,
        cube_repo_id: Option<&str>,
        error_text: &str,
        retry_delays: &[Duration],
    ) -> Result<(WorkExecution, WorkRun, PreStartFailureOutcome)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if execution.status != "ready" {
            bail!(
                "execution {execution_id} is not ready and cannot record pre-start failure \
                 from status `{}`",
                execution.status
            );
        }

        let now = now_string();
        let new_count = execution.pre_start_failure_count + 1;
        let max_retries = retry_delays.len() as i64;

        let run_id = next_id("run");
        tx.execute(
            "INSERT INTO work_runs (
                id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                artifacts_path, created_at, started_at, finished_at
             ) VALUES (?1, ?2, ?3, 'failed', ?4, NULL, NULL, NULL, ?5, ?5, ?5)",
            params![run_id, execution_id, agent_id, error_text, now],
        )?;

        let outcome = if new_count <= max_retries {
            let delay = retry_delays[(new_count - 1) as usize];
            let dispatch_not_before = (SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                + delay.as_secs())
            .to_string();
            tx.execute(
                "UPDATE work_executions
                 SET pre_start_failure_count = ?2,
                     cube_repo_id = COALESCE(?3, cube_repo_id),
                     cube_lease_id = NULL,
                     cube_workspace_id = NULL,
                     workspace_path = NULL,
                     started_at = NULL,
                     finished_at = NULL,
                     dispatch_not_before = ?4
                 WHERE id = ?1",
                params![execution_id, new_count, cube_repo_id, dispatch_not_before],
            )?;
            PreStartFailureOutcome::Retry { delay }
        } else {
            tx.execute(
                "UPDATE work_executions
                 SET status = 'failed',
                     pre_start_failure_count = ?2,
                     cube_repo_id = COALESCE(?3, cube_repo_id),
                     cube_lease_id = NULL,
                     cube_workspace_id = NULL,
                     workspace_path = NULL,
                     started_at = COALESCE(started_at, ?4),
                     finished_at = ?4
                 WHERE id = ?1",
                params![execution_id, new_count, cube_repo_id, now],
            )?;
            PreStartFailureOutcome::PermanentFail
        };

        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        let run = query_run(&tx, &run_id)?
            .with_context(|| format!("missing run after insert: {run_id}"))?;
        tx.commit()?;
        Ok((execution, run, outcome))
    }

    pub fn finish_execution_run(
        &self,
        execution_id: &str,
        run_id: &str,
        execution_status: &str,
        run_status: &str,
        result_summary: Option<&str>,
        error_text: Option<&str>,
        clear_workspace_lease: bool,
        attention: Option<CreateAttentionItemInput>,
    ) -> Result<(WorkExecution, WorkRun, Option<WorkAttentionItem>)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if execution.status != "running" {
            bail!(
                "execution {execution_id} is not running and cannot finish a run from status `{}`",
                execution.status
            );
        }

        let run = query_run(&tx, run_id).require("run", run_id)?;
        if run.execution_id != execution_id {
            bail!("run {run_id} does not belong to execution {execution_id}");
        }
        if run.status != "active" {
            bail!(
                "run {run_id} is not active and cannot be finished from status `{}`",
                run.status
            );
        }

        let now = now_string();
        let execution_finished_at = if execution_status_is_terminal(execution_status) {
            Some(now.as_str())
        } else {
            None
        };
        let normalized_result_summary = normalize_optional_text(result_summary.map(str::to_owned));
        let normalized_error_text = normalize_optional_text(error_text.map(str::to_owned));

        tx.execute(
            "UPDATE work_executions
             SET status = ?2,
                 cube_lease_id = CASE WHEN ?3 THEN NULL ELSE cube_lease_id END,
                 cube_workspace_id = CASE WHEN ?3 THEN NULL ELSE cube_workspace_id END,
                 workspace_path = CASE WHEN ?3 THEN NULL ELSE workspace_path END,
                 finished_at = ?4
             WHERE id = ?1",
            params![
                execution_id,
                execution_status,
                clear_workspace_lease,
                execution_finished_at,
            ],
        )?;

        tx.execute(
            "UPDATE work_runs
             SET status = ?2,
                 error_text = ?3,
                 result_summary = ?4,
                 finished_at = ?5
             WHERE id = ?1",
            params![
                run_id,
                run_status,
                normalized_error_text,
                normalized_result_summary,
                now,
            ],
        )?;

        let attention_item = if let Some(input) = attention {
            // `finish_execution_run` only ever attaches to the
            // execution it just finished. The caller threading a
            // `work_item_id` instead is a bug — the work-item-scoped
            // attention path goes through `create_attention_item`.
            if input.work_item_id.is_some() {
                bail!(
                    "finish_execution_run attention payload must not set work_item_id (got {:?})",
                    input.work_item_id
                );
            }
            let provided = input.execution_id.as_deref().unwrap_or(execution_id);
            if provided != execution_id {
                bail!(
                    "attention item execution `{provided}` does not match finished execution `{execution_id}`",
                );
            }

            let attention_id = next_id("attn");
            let status = input.status.unwrap_or_else(|| "open".to_owned());
            let resolved_at = normalize_optional_text(input.resolved_at);
            tx.execute(
                "INSERT INTO work_attention_items (
                    id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at
                 ) VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    attention_id,
                    execution_id,
                    input.kind,
                    status,
                    input.title,
                    input.body_markdown,
                    now,
                    resolved_at,
                ],
            )?;

            Some(
                query_attention_item(&tx, &attention_id)?.with_context(|| {
                    format!("missing attention item after insert: {attention_id}")
                })?,
            )
        } else {
            None
        };

        let execution = query_execution(&tx, execution_id).require("execution", execution_id)?;
        let run = query_run(&tx, run_id).require("run", run_id)?;
        tx.commit()?;
        Ok((execution, run, attention_item))
    }

    pub fn create_run(&self, input: CreateRunInput) -> Result<WorkRun> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_execution_exists(&tx, &input.execution_id)?;

        let id = next_id("run");
        let now = now_string();
        let status = input.status.unwrap_or_else(|| "starting".to_owned());
        let error_text = normalize_optional_text(input.error_text);
        let result_summary = normalize_optional_text(input.result_summary);
        let transcript_path = normalize_optional_text(input.transcript_path);
        let artifacts_path = normalize_optional_text(input.artifacts_path);
        let started_at = normalize_optional_text(input.started_at);
        let finished_at = normalize_optional_text(input.finished_at);

        tx.execute(
            "INSERT INTO work_runs (
                id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                artifacts_path, created_at, started_at, finished_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                id,
                input.execution_id,
                input.agent_id,
                status,
                error_text,
                result_summary,
                transcript_path,
                artifacts_path,
                now,
                started_at,
                finished_at,
            ],
        )?;

        let run =
            query_run(&tx, &id)?.with_context(|| format!("missing run after insert: {id}"))?;
        tx.commit()?;
        Ok(run)
    }

    pub fn list_runs(&self, execution_id: &str) -> Result<Vec<WorkRun>> {
        let conn = self.connect()?;
        ensure_execution_exists(&conn, execution_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                    artifacts_path, created_at, started_at, finished_at
             FROM work_runs
             WHERE execution_id = ?1
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([execution_id], map_run)?;
        collect_rows(rows)
    }

    pub fn get_run(&self, id: &str) -> Result<WorkRun> {
        let conn = self.connect()?;
        query_run(&conn, id).require("run", id)
    }

    /// Persist the verbatim `transcript_path` we learned from a hook
    /// event payload.
    ///
    /// **Namespace warning.** The dispatcher's `_boss_run_id` carries
    /// the `work_executions.id` (`exec_*`), not a `work_runs.id`
    /// (`run_*`) — `runner.rs::run_execution` plumbs `execution.id`
    /// through to `BOSS_RUN_ID` for the worker shim, and the engine's
    /// `WorkerRegistry` keys its slot map on the same identifier. The
    /// pre-2026-05-12 version of this function joined `WHERE id = ?1`
    /// on `work_runs.id`, which never matched — every hook quietly
    /// returned "0 rows updated" and the `transcript_path` column
    /// stayed NULL forever. PR #366 and PR #372 both shipped trying
    /// to fix the symptom without spotting the cross-namespace join.
    /// This implementation resolves the most-recent `work_runs` row
    /// for the execution and writes against its `id`, so the caller
    /// can keep handing us an execution id without worrying about the
    /// run/execution split.
    ///
    /// The lookup picks the latest run per `(created_at DESC, id
    /// DESC)`: an execution can have multiple `work_runs` rows from
    /// re-spawns, but only one is "live" at any moment (the others
    /// are terminal). The live one is always the most recent insert,
    /// so writing to it lines up with the running worker's actual
    /// transcript file.
    ///
    /// Idempotent for the first writer per run (the
    /// `WHERE transcript_path IS NULL` clause keeps every subsequent
    /// hook event from rewriting the same value, and also keeps a
    /// later SessionStart/resume from clobbering the path the
    /// summarizer's tail watcher has already opened).
    ///
    /// Returns:
    /// - `Updated` — the row's `transcript_path` was just written.
    /// - `AlreadySet` — the latest run for this execution already
    ///   has a non-NULL `transcript_path`; legitimate steady-state
    ///   no-op.
    /// - `RowMissing` — no `work_runs` row exists yet for this
    ///   execution. Split out from `AlreadySet` because that
    ///   conflation is precisely what hid the wrong-namespace bug:
    ///   on the wire, "0 rows updated" looked identical between
    ///   "run already populated" and "the join never matched in the
    ///   first place".
    pub fn set_run_transcript_path_if_unset(
        &self,
        execution_id: &str,
        transcript_path: &str,
    ) -> Result<SetRunTranscriptPathOutcome> {
        let conn = self.connect()?;
        let latest_run_id: Option<String> = conn
            .query_row(
                "SELECT id FROM work_runs
                 WHERE execution_id = ?1
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1",
                params![execution_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(run_id) = latest_run_id else {
            return Ok(SetRunTranscriptPathOutcome::RowMissing);
        };
        let updated = conn.execute(
            "UPDATE work_runs
             SET transcript_path = ?2
             WHERE id = ?1 AND transcript_path IS NULL",
            params![run_id, transcript_path],
        )?;
        if updated > 0 {
            Ok(SetRunTranscriptPathOutcome::Updated)
        } else {
            Ok(SetRunTranscriptPathOutcome::AlreadySet)
        }
    }

    /// Read-side companion to [`set_run_transcript_path_if_unset`].
    ///
    /// **Namespace warning — same trap as the write side.** Every
    /// caller in the engine that previously did
    /// `work_db.get_run(run_id).transcript_path` was actually handing
    /// in an `exec_*` (`work_executions.id`) and joining it against
    /// `work_runs.id`, so the lookup never matched and the path
    /// stayed NULL on the wire. The write-side path was fixed in PR
    /// #384; the read side kept the same shape, which is why
    /// `bossctl live-status debug --json` reported `transcript_path:
    /// null` for live slots even when the underlying `work_runs` row
    /// had the column populated. This helper closes that gap by
    /// keying on `execution_id` and resolving the latest `work_runs`
    /// row the same way the write side does (`ORDER BY created_at
    /// DESC, id DESC LIMIT 1`).
    ///
    /// Returns `Ok(None)` when either the execution has no
    /// `work_runs` row yet, or the latest row's `transcript_path`
    /// column is still NULL — both are legitimate steady states
    /// while a worker is still booting. Returns `Err` only on a real
    /// SQL failure; callers should log-and-default rather than abort.
    pub fn transcript_path_for_execution(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        let path: Option<Option<String>> = conn
            .query_row(
                "SELECT transcript_path FROM work_runs
                 WHERE execution_id = ?1
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1",
                params![execution_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(path.flatten())
    }

    /// Host id of the most-recent `work_runs` row for `execution_id`,
    /// or `None` when the execution has no run yet.
    ///
    /// The distributed-execution dispatch path stamps `host_id` on the
    /// run at start (`'local'` for local runs, the scheduler-selected
    /// id for remote ones). The transcript-tail RPC reads this to decide
    /// whether the recorded `transcript_path` lives on the local
    /// filesystem (`host_id = 'local'`) or must be pulled over SSH, and
    /// the live-status dispatcher reads it to decide whether a slotless
    /// run is a remote worker that warrants a virtual slot. Resolves the
    /// latest run the same way [`Self::transcript_path_for_execution`]
    /// does (`ORDER BY created_at DESC, id DESC`), so it always reflects
    /// the live run after a re-spawn.
    pub fn latest_run_host_for_execution(&self, execution_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        let host: Option<String> = conn
            .query_row(
                "SELECT host_id FROM work_runs
                 WHERE execution_id = ?1
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1",
                params![execution_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(host)
    }

    /// Persist the remote worker pid onto the latest `work_runs` row for
    /// `execution_id`. The SSH spawn path captures the pid from the
    /// wrapper handshake (`parse_remote_pid`) and stamps it here so the
    /// design's "Storage Additions" `work_runs.remote_pid` — the
    /// addressing key for control-channel signal delivery — is durable.
    ///
    /// Mirrors [`Self::set_run_transcript_path_if_unset`]'s namespace
    /// handling: `execution_id` is the `exec_*` id the spawn path holds,
    /// resolved to the live `work_runs.id`. Returns `true` when a row was
    /// updated, `false` when no run exists yet (benign — the caller logs
    /// and moves on; the pid is informational, not a spawn precondition).
    pub fn set_run_remote_pid_for_execution(
        &self,
        execution_id: &str,
        remote_pid: i64,
    ) -> Result<bool> {
        let conn = self.connect()?;
        let updated = conn.execute(
            "UPDATE work_runs
             SET remote_pid = ?2
             WHERE id = (
                 SELECT id FROM work_runs
                 WHERE execution_id = ?1
                 ORDER BY created_at DESC, id DESC
                 LIMIT 1
             )",
            params![execution_id, remote_pid],
        )?;
        Ok(updated > 0)
    }

    /// Active runs on a non-local host whose backing execution is still
    /// non-terminal — the set of detached remote workers the engine
    /// should re-attach to after a restart.
    ///
    /// A remote worker is launched detached (`nohup`) and survives the
    /// engine restarting, but the reverse events-socket forward that
    /// carries its hook stream rides the engine's `ControlMaster` and
    /// dies with the old engine process. On startup the engine queries
    /// this set and re-establishes each forward (see
    /// [`crate::remote_reattach`]) so the still-running worker's events
    /// — and its eventual `Stop` / PR-URL completion — reach the engine
    /// again. Local runs are excluded (`host_id != 'local'`): a local
    /// worker is a child of the previous engine and is already gone.
    /// Terminal executions are excluded so a settled run is never
    /// re-attached.
    pub fn list_reattachable_remote_runs(&self) -> Result<Vec<RemoteRunHandle>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT r.id, r.execution_id, r.host_id, r.remote_pid
             FROM work_runs r
             JOIN work_executions e ON e.id = r.execution_id
             WHERE r.status = 'active'
               AND r.host_id != 'local'
               AND e.status NOT IN
                   ('completed', 'failed', 'abandoned', 'cancelled', 'orphaned')
             ORDER BY r.created_at ASC, r.id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(RemoteRunHandle {
                run_id: row.get(0)?,
                execution_id: row.get(1)?,
                host_id: row.get(2)?,
                remote_pid: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Test-only helper: force `transcript_path` back to NULL on an
    /// existing row. Used by the dispatcher regression test to model
    /// the production race where a SessionStart's payload-driven
    /// persist fired against a work_runs row that did not exist
    /// yet, leaving the column NULL after the row was later
    /// inserted. The cache fallback (this PR) is what allows a
    /// subsequent hook to finally win.
    #[cfg(test)]
    pub fn force_updated_at_for_test(&self, work_item_id: &str, epoch_secs: i64) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE tasks SET updated_at = ?2 WHERE id = ?1",
            params![work_item_id, epoch_secs.to_string()],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn insert_terminal_execution_for_test(
        &self,
        work_item_id: &str,
        status: &str,
        created_at_epoch: i64,
    ) -> Result<()> {
        let conn = self.connect()?;
        let id = format!("exec-test-{}-{}", work_item_id, created_at_epoch);
        conn.execute(
            "INSERT INTO work_executions
               (id, work_item_id, kind, status, repo_remote_url,
                priority, created_at)
             VALUES (?1, ?2, 'chore_implementation', ?3,
                     'https://github.com/test/repo', 0, ?4)",
            params![id, work_item_id, status, created_at_epoch.to_string()],
        )?;
        Ok(())
    }

    /// Mark a task `done` without running `cascade_dependents_after_prereq_status_change`.
    /// Used in tests that need to simulate the engine being offline when a
    /// prereq transitions, so the sweeper can be exercised as the recovery path.
    #[cfg(test)]
    pub fn mark_task_done_for_test_no_cascade(&self, task_id: &str) -> Result<()> {
        let conn = self.connect()?;
        let now = now_string();
        conn.execute(
            "UPDATE tasks
             SET status = 'done', last_status_actor = 'engine', updated_at = ?2
             WHERE id = ?1 AND deleted_at IS NULL",
            params![task_id, now],
        )?;
        Ok(())
    }

    /// Overwrite `last_status_actor` for a task without touching any other
    /// column. Used in tests to simulate a concurrent update that reset the
    /// actor (the scenario that previously caused the cascade to skip an item).
    #[cfg(test)]
    pub fn force_last_status_actor_for_test(&self, task_id: &str, actor: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE tasks SET last_status_actor = ?2 WHERE id = ?1",
            params![task_id, actor],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn force_execution_status_for_test(&self, work_item_id: &str, status: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE work_executions SET status = ?2 WHERE work_item_id = ?1",
            params![work_item_id, status],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn force_task_status_for_test(&self, task_id: &str, status: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE tasks SET status = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![task_id, status],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn force_started_at_for_test(&self, execution_id: &str, epoch_secs: i64) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE work_executions SET started_at = ?2 WHERE id = ?1",
            params![execution_id, epoch_secs.to_string()],
        )?;
        Ok(())
    }

    pub fn force_transient_failure_count_for_test(
        &self,
        execution_id: &str,
        count: i64,
    ) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE work_executions SET transient_failure_count = ?2 WHERE id = ?1",
            params![execution_id, count],
        )?;
        Ok(())
    }

    /// Overwrite `branch_naming` for an execution row. Used in tests to
    /// verify that the detector reconstructs the correct branch name from
    /// the snapshotted strategy without needing to re-create the full
    /// product/editorial-rules fixture.
    #[cfg(test)]
    pub fn force_branch_naming_for_test(
        &self,
        execution_id: &str,
        naming: &BranchNaming,
    ) -> Result<()> {
        let json = serde_json::to_string(naming)
            .with_context(|| format!("failed to serialise BranchNaming for {execution_id}"))?;
        let conn = self.connect()?;
        conn.execute(
            "UPDATE work_executions SET branch_naming = ?2 WHERE id = ?1",
            params![execution_id, json],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn clear_run_transcript_path_for_test(&self, run_id: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE work_runs SET transcript_path = NULL WHERE id = ?1",
            params![run_id],
        )?;
        Ok(())
    }

    /// Stamp the actual pane-slot identity onto an existing run record.
    /// The coordinator inserts the run with the worker-pool placeholder
    /// (`worker-N` from capacity tracking), then calls this once the
    /// app has reported the real slot allocation back from
    /// `SpawnWorkerPane`. After this point `agent_id` is treated as
    /// immutable for the run's lifetime — re-spawning into a different
    /// slot would create a new run rather than mutate this one.
    pub fn set_run_agent_id(&self, run_id: &str, agent_id: &str) -> Result<WorkRun> {
        let conn = self.connect()?;
        let updated = conn.execute(
            "UPDATE work_runs SET agent_id = ?2 WHERE id = ?1",
            params![run_id, agent_id],
        )?;
        if updated == 0 {
            bail!("unknown run: {run_id}");
        }
        query_run(&conn, run_id).require("run", run_id)
    }
}
