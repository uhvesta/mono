use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};

pub use boss_protocol::{
    AddDependencyInput, CreateAttentionItemInput, CreateChoreInput, CreateExecutionInput,
    CreateManyChoresInput, CreateManyTasksInput, CreateProductInput, CreateProjectInput,
    CreateRunInput, CreateTaskInput, DependencyDirection, DependencyEdge, DependencyFilter,
    ExecutionReconcileResult, ListDependenciesInput, Product, Project, RemoveDependencyInput,
    RequestExecutionInput, Task, TaskRuntime, WorkAttentionItem, WorkExecution, WorkItem,
    WorkItemDependency, WorkItemDependencyDetail, WorkItemDependencyView, WorkItemPatch, WorkRun,
    WorkTree,
};

use crate::work_dependencies::{self as deps, EdgeInsertOutcome, RELATION_BLOCKS};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct WorkDb {
    path: PathBuf,
}

impl WorkDb {
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create work db directory {}", parent.display())
            })?;
        }

        let db = Self { path };
        db.init()?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn list_products(&self) -> Result<Vec<Product>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, slug, description, repo_remote_url, status, created_at, updated_at
             FROM products
             ORDER BY name COLLATE NOCASE ASC",
        )?;
        let rows = stmt.query_map([], map_product)?;
        collect_rows(rows)
    }

    pub fn create_product(&self, input: CreateProductInput) -> Result<Product> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;

        let id = next_id("prod");
        let now = now_string();
        let slug = unique_product_slug(&tx, &slugify(&input.name))?;
        let description = input.description.unwrap_or_default();
        let repo_remote_url = normalize_optional_text(input.repo_remote_url);

        tx.execute(
            "INSERT INTO products (id, name, slug, description, repo_remote_url, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, ?6)",
            params![id, input.name, slug, description, repo_remote_url, now],
        )?;

        let product = query_product(&tx, &id)?
            .with_context(|| format!("missing product after insert: {id}"))?;
        tx.commit()?;
        Ok(product)
    }

    pub fn list_projects(
        &self,
        product_id: &str,
        dep_filter: Option<&DependencyFilter>,
    ) -> Result<Vec<Project>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at, last_status_actor
             FROM projects
             WHERE product_id = ?1
             ORDER BY created_at ASC, name COLLATE NOCASE ASC",
        )?;
        let rows = stmt.query_map([product_id], map_project)?;
        let mut projects: Vec<Project> = collect_rows(rows)?;
        if let Some(filter) = dep_filter {
            apply_dep_filter(&conn, filter, |project: &Project| project.id.as_str(), |project: &Project| project.status.as_str(), &mut projects)?;
        }
        Ok(projects)
    }

    pub fn create_project(&self, input: CreateProjectInput) -> Result<Project> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_product_exists(&tx, &input.product_id)?;

        let id = next_id("proj");
        let now = now_string();
        let slug = unique_project_slug(&tx, &input.product_id, &slugify(&input.name))?;
        let description = input.description.unwrap_or_default();
        let goal = input.goal.unwrap_or_default();

        tx.execute(
            "INSERT INTO projects (id, product_id, name, slug, description, goal, status, priority, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'planned', 'medium', ?7, ?7)",
            params![id, input.product_id, input.name, slug, description, goal, now],
        )?;

        // Auto-create the project's design task. The design phase is
        // a unit of work just like any other task on the project, so
        // we represent it as one — the kanban renders it through the
        // existing task pipeline (drag/drop, popover, runtime dot,
        // PR-on-merge round-trip). It sorts first via `ordinal = 0`
        // so the dispatcher picks it up before the project's own
        // tasks (which start at `ordinal = 1` per the
        // task-creation default).
        insert_design_task_for_project_in_tx(&tx, &input.product_id, &id, input.autostart)?;

        let project = query_project(&tx, &id)?
            .with_context(|| format!("missing project after insert: {id}"))?;
        tx.commit()?;
        Ok(project)
    }

    pub fn create_task(&self, input: CreateTaskInput) -> Result<Task> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let task = insert_task_in_tx(&tx, input)?;
        tx.commit()?;
        Ok(task)
    }

    pub fn create_chore(&self, input: CreateChoreInput) -> Result<Task> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let chore = insert_chore_in_tx(&tx, input)?;
        tx.commit()?;
        Ok(chore)
    }

    /// Insert N tasks atomically. The whole batch is wrapped in a
    /// single sqlite transaction; any per-item validation failure
    /// rolls back the entire batch (no partial state). Errors are
    /// annotated with the offending item index so the CLI can map
    /// them back to the input file.
    pub fn create_many_tasks(&self, input: CreateManyTasksInput) -> Result<Vec<Task>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut created = Vec::with_capacity(input.items.len());
        for (index, item) in input.items.into_iter().enumerate() {
            let task = insert_task_in_tx(&tx, item).with_context(|| format!("item {index}"))?;
            created.push(task);
        }
        tx.commit()?;
        Ok(created)
    }

    /// Insert N chores atomically. See [`Self::create_many_tasks`] for
    /// atomicity contract.
    pub fn create_many_chores(&self, input: CreateManyChoresInput) -> Result<Vec<Task>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut created = Vec::with_capacity(input.items.len());
        for (index, item) in input.items.into_iter().enumerate() {
            let chore = insert_chore_in_tx(&tx, item).with_context(|| format!("item {index}"))?;
            created.push(chore);
        }
        tx.commit()?;
        Ok(created)
    }

    pub fn create_execution(&self, input: CreateExecutionInput) -> Result<WorkExecution> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = insert_execution(&tx, input)?;
        tx.commit()?;
        Ok(execution)
    }

    /// Returns or creates a ready execution for `work_item_id`, applying any
    /// priority / preferred-workspace overrides from the request.
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
        input: RequestExecutionInput,
        is_live: F,
    ) -> Result<WorkExecution> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = request_execution_in_tx_with_live_check(&tx, input, is_live)?;
        tx.commit()?;
        Ok(execution)
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
    /// Returns the work item ids that were demoted. Items whose
    /// executions already produced a run (active worker that crashed,
    /// terminated cleanly, or is still executing) are left alone —
    /// `reconcile_active_dispatch` handles those via re-dispatch.
    pub fn heal_ghost_active_chores(&self) -> Result<Vec<String>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let candidate_ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT t.id FROM tasks t
                 WHERE t.status = 'active'
                   AND t.deleted_at IS NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM work_runs wr
                       JOIN work_executions we ON wr.execution_id = we.id
                       WHERE we.work_item_id = t.id
                   )",
            )?;
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut healed = Vec::new();
        let now = now_string();
        for work_item_id in candidate_ids {
            // Abandon any non-terminal executions so they don't get
            // picked up by the dispatcher after the demote. Terminal
            // executions are left alone — they're already settled.
            tx.execute(
                "UPDATE work_executions
                 SET status = 'abandoned',
                     finished_at = COALESCE(finished_at, ?2)
                 WHERE work_item_id = ?1
                   AND status NOT IN ('completed', 'failed', 'abandoned', 'cancelled')",
                params![work_item_id, now],
            )?;
            // Demote the kanban status. Use a guarded update so we
            // don't race a concurrent move to `done`/`archived`.
            let updated = tx.execute(
                "UPDATE tasks
                 SET status = 'todo',
                     updated_at = ?2
                 WHERE id = ?1
                   AND status = 'active'
                   AND deleted_at IS NULL",
                params![work_item_id, now],
            )?;
            if updated > 0 {
                healed.push(work_item_id);
            }
        }
        tx.commit()?;
        Ok(healed)
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
            let needs_dispatch = match query_latest_execution_for_work_item(&tx, &work_item_id)? {
                Some(existing) => {
                    execution_status_is_terminal(&existing.status) || !is_live(&existing.id)
                }
                None => true,
            };
            if !needs_dispatch {
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

    pub fn list_executions(&self, work_item_id: Option<&str>) -> Result<Vec<WorkExecution>> {
        let conn = self.connect()?;
        if let Some(work_item_id) = work_item_id {
            let _ = product_id_for_work_item(&conn, work_item_id)?;
            let mut stmt = conn.prepare(
                "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                        cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                        created_at, started_at, finished_at
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
                    created_at, started_at, finished_at
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

    /// Fetch a single project by id. Used by the runner when it
    /// composes the worker prompt for a `kind = 'design'` task —
    /// the design task itself is sparse, so the runner enriches the
    /// prompt with the parent project's name/goal/description.
    pub fn get_project(&self, id: &str) -> Result<Project> {
        let conn = self.connect()?;
        query_project(&conn, id)?.with_context(|| format!("unknown project: {id}"))
    }

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
        let existing = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
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
                    created_at, started_at, finished_at
             FROM work_executions
             WHERE status = 'ready'
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
                    created_at, started_at, finished_at
             FROM work_executions
             WHERE status NOT IN ('completed', 'failed', 'abandoned', 'cancelled')
               AND cube_lease_id IS NOT NULL
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], map_execution)?;
        collect_rows(rows)
    }

    pub fn reconcile_product_executions(
        &self,
        product_id: &str,
    ) -> Result<ExecutionReconcileResult> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let product = query_product(&tx, product_id)?
            .with_context(|| format!("unknown product: {product_id}"))?;
        let _projects = list_projects_for_product(&tx, product_id)?;
        let tasks = list_tasks_for_product(&tx, product_id)?;
        let mut result = ExecutionReconcileResult::default();

        let repo_remote_url = product.repo_remote_url.clone();

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
            match task.kind.as_str() {
                "chore" => {
                    if task_accepts_execution(&task) {
                        reconcile_work_item_execution(
                            &tx,
                            &mut result,
                            &task.id,
                            "chore_implementation",
                            "ready",
                            repo_remote_url.as_deref(),
                        )?;
                    }
                }
                "project_task" | "design" => {
                    if let Some(project_id) = &task.project_id {
                        project_tasks
                            .entry(project_id.clone())
                            .or_default()
                            .push(task);
                    }
                }
                _ => {}
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

            let first_incomplete = tasks.iter().position(|task| task_accepts_execution(task));

            for (index, task) in tasks.iter().enumerate() {
                if !task_accepts_execution(task) {
                    continue;
                }
                let desired_status = if Some(index) == first_incomplete {
                    "ready"
                } else {
                    "waiting_dependency"
                };
                let execution_kind = match task.kind.as_str() {
                    "design" => "project_design",
                    _ => "task_implementation",
                };
                reconcile_work_item_execution(
                    &tx,
                    &mut result,
                    &task.id,
                    execution_kind,
                    desired_status,
                    repo_remote_url.as_deref(),
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
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
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
                 started_at = COALESCE(started_at, ?6),
                 finished_at = NULL
             WHERE id = ?1",
            params![
                execution_id,
                cube_repo_id,
                cube_lease_id,
                cube_workspace_id,
                workspace_path,
                now
            ],
        )?;

        // Auto-advance the work item's kanban status to `active` so
        // the card moves into the Doing column when work begins.
        // Only applies to tasks/chores; products and projects use a
        // different status vocabulary and aren't rendered on the
        // kanban. Don't downgrade items already in `done` or
        // `archived` — manual transitions win.
        tx.execute(
            "UPDATE tasks
             SET status = 'active',
                 updated_at = ?2
             WHERE id = ?1
               AND deleted_at IS NULL
               AND status NOT IN ('done', 'archived')",
            params![execution.work_item_id, now],
        )?;

        let run_id = next_id("run");
        tx.execute(
            "INSERT INTO work_runs (
                id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                artifacts_path, created_at, started_at, finished_at
             ) VALUES (?1, ?2, ?3, 'active', NULL, NULL, NULL, NULL, ?4, ?4, NULL)",
            params![run_id, execution_id, agent_id, now],
        )?;

        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
        let run = query_run(&tx, &run_id)?
            .with_context(|| format!("missing run after insert: {run_id}"))?;
        tx.commit()?;
        Ok((execution, run))
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
        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
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

        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
        let run = query_run(&tx, &run_id)?
            .with_context(|| format!("missing run after insert: {run_id}"))?;
        tx.commit()?;
        Ok((execution, run))
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
        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
        if execution.status != "running" {
            bail!(
                "execution {execution_id} is not running and cannot finish a run from status `{}`",
                execution.status
            );
        }

        let run = query_run(&tx, run_id)?.with_context(|| format!("unknown run: {run_id}"))?;
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
            if input.execution_id != execution_id {
                bail!(
                    "attention item execution `{}` does not match finished execution `{execution_id}`",
                    input.execution_id
                );
            }

            let attention_id = next_id("attn");
            let status = input.status.unwrap_or_else(|| "open".to_owned());
            let resolved_at = normalize_optional_text(input.resolved_at);
            tx.execute(
                "INSERT INTO work_attention_items (
                    id, execution_id, kind, status, title, body_markdown, created_at, resolved_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
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

        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
        let run = query_run(&tx, run_id)?.with_context(|| format!("unknown run: {run_id}"))?;
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
        query_run(&conn, id)?.with_context(|| format!("unknown run: {id}"))
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
        query_run(&conn, run_id)?.with_context(|| format!("unknown run: {run_id}"))
    }

    pub fn create_attention_item(
        &self,
        input: CreateAttentionItemInput,
    ) -> Result<WorkAttentionItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_execution_exists(&tx, &input.execution_id)?;

        let id = next_id("attn");
        let now = now_string();
        let status = input.status.unwrap_or_else(|| "open".to_owned());
        let resolved_at = normalize_optional_text(input.resolved_at);

        tx.execute(
            "INSERT INTO work_attention_items (
                id, execution_id, kind, status, title, body_markdown, created_at, resolved_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                input.execution_id,
                input.kind,
                status,
                input.title,
                input.body_markdown,
                now,
                resolved_at,
            ],
        )?;

        let item = query_attention_item(&tx, &id)?
            .with_context(|| format!("missing attention item after insert: {id}"))?;
        tx.commit()?;
        Ok(item)
    }

    pub fn list_attention_items(&self, execution_id: &str) -> Result<Vec<WorkAttentionItem>> {
        let conn = self.connect()?;
        ensure_execution_exists(&conn, execution_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, execution_id, kind, status, title, body_markdown, created_at, resolved_at
             FROM work_attention_items
             WHERE execution_id = ?1
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([execution_id], map_attention_item)?;
        collect_rows(rows)
    }

    pub fn get_attention_item(&self, id: &str) -> Result<WorkAttentionItem> {
        let conn = self.connect()?;
        query_attention_item(&conn, id)?.with_context(|| format!("unknown attention item: {id}"))
    }

    pub fn update_work_item(&self, id: &str, patch: WorkItemPatch) -> Result<WorkItem> {
        match classify_id(id)? {
            ItemKind::Product => self.update_product(id, patch),
            ItemKind::Project => self.update_project(id, patch),
            ItemKind::Task => self.update_task(id, patch),
        }
    }

    pub fn delete_work_item(&self, id: &str) -> Result<()> {
        match classify_id(id)? {
            ItemKind::Task => {
                let mut conn = self.connect()?;
                let tx = conn.transaction()?;
                let now = now_string();
                let rows = tx.execute(
                    "UPDATE tasks SET deleted_at = ?2, updated_at = ?2
                     WHERE id = ?1 AND deleted_at IS NULL",
                    params![id, now],
                )?;
                if rows == 0 {
                    bail!("unknown task: {id}");
                }
                // Q10 (deleted prereq): drop every dependency edge that
                // names this task as either endpoint. A row with a
                // tombstoned prerequisite is the worst of both worlds —
                // dependents stuck on a row that is no longer a thing.
                tx.execute(
                    "DELETE FROM work_item_dependencies
                     WHERE dependent_id = ?1 OR prerequisite_id = ?1",
                    params![id],
                )?;
                tx.commit()?;
                Ok(())
            }
            ItemKind::Product => bail!("product deletion is not supported; archive it instead"),
            ItemKind::Project => bail!("project deletion is not supported; archive it instead"),
        }
    }

    pub fn get_work_tree(&self, product_id: &str) -> Result<WorkTree> {
        let conn = self.connect()?;
        let product = query_product(&conn, product_id)?
            .with_context(|| format!("unknown product: {product_id}"))?;

        let projects = {
            let mut stmt = conn.prepare(
                "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at, last_status_actor
                 FROM projects
                 WHERE product_id = ?1
                 ORDER BY created_at ASC, name COLLATE NOCASE ASC",
            )?;
            let rows = stmt.query_map([product_id], map_project)?;
            collect_rows(rows)?
        };

        let tasks = {
            // `kind IN ('project_task', 'design')` — the design task
            // auto-created at project birth lives in the same lane as
            // every other project task. Sorting on `ordinal` ASC puts
            // the design task (ordinal = 0) at the head of the
            // project's task chain, which matches the kanban
            // expectation that design lands first.
            let mut stmt = conn.prepare(
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority
                 FROM tasks
                 WHERE product_id = ?1 AND kind IN ('project_task', 'design') AND deleted_at IS NULL
                 ORDER BY COALESCE(ordinal, 0) ASC, created_at ASC",
            )?;
            let rows = stmt.query_map([product_id], map_task)?;
            collect_rows(rows)?
        };

        let chores = {
            let mut stmt = conn.prepare(
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority
                 FROM tasks
                 WHERE product_id = ?1 AND kind = 'chore' AND deleted_at IS NULL
                 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map([product_id], map_task)?;
            collect_rows(rows)?
        };

        let task_runtimes = collect_task_runtimes(&conn, &tasks, &chores)?;
        let dependencies = collect_product_dependencies(&conn, product_id)?;

        Ok(WorkTree {
            product,
            projects,
            tasks,
            chores,
            task_runtimes,
            dependencies,
        })
    }

    pub fn reorder_project_tasks(&self, project_id: &str, task_ids: &[String]) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_project_exists(&tx, project_id)?;

        let mut existing = {
            let mut stmt = tx.prepare(
                "SELECT id
                 FROM tasks
                 WHERE project_id = ?1 AND kind = 'project_task' AND deleted_at IS NULL
                 ORDER BY COALESCE(ordinal, 0) ASC, created_at ASC",
            )?;
            let rows = stmt.query_map([project_id], |row| row.get::<_, String>(0))?;
            collect_rows(rows)?
        };
        let mut requested = task_ids.to_vec();
        existing.sort();
        requested.sort();
        if existing != requested {
            bail!("reorder request must include the full active task set for the project");
        }

        for (index, task_id) in task_ids.iter().enumerate() {
            tx.execute(
                "UPDATE tasks SET ordinal = ?2, updated_at = ?3 WHERE id = ?1",
                params![task_id, (index as i64) + 1, now_string()],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    pub fn get_work_item(&self, id: &str) -> Result<WorkItem> {
        let conn = self.connect()?;
        match classify_id(id)? {
            ItemKind::Product => query_product(&conn, id)?
                .map(WorkItem::Product)
                .with_context(|| format!("unknown product: {id}")),
            ItemKind::Project => query_project(&conn, id)?
                .map(WorkItem::Project)
                .with_context(|| format!("unknown project: {id}")),
            ItemKind::Task => query_task(&conn, id)?
                .filter(|task| task.deleted_at.is_none())
                .map(task_to_item)
                .with_context(|| format!("unknown task: {id}")),
        }
    }

    pub fn list_tasks(
        &self,
        product_id: &str,
        project_id: Option<&str>,
        dep_filter: Option<&DependencyFilter>,
    ) -> Result<Vec<Task>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;

        let mut tasks = if let Some(project_id) = project_id {
            ensure_project_belongs_to_product(&conn, project_id, product_id)?;
            let mut stmt = conn.prepare(
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority
                 FROM tasks
                 WHERE product_id = ?1 AND project_id = ?2 AND kind IN ('project_task', 'design') AND deleted_at IS NULL
                 ORDER BY COALESCE(ordinal, 0) ASC, created_at ASC",
            )?;
            let rows = stmt.query_map(params![product_id, project_id], map_task)?;
            collect_rows(rows)?
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority
                 FROM tasks
                 WHERE product_id = ?1 AND kind IN ('project_task', 'design') AND deleted_at IS NULL
                 ORDER BY COALESCE(ordinal, 0) ASC, created_at ASC",
            )?;
            let rows = stmt.query_map([product_id], map_task)?;
            collect_rows(rows)?
        };

        if let Some(filter) = dep_filter {
            apply_dep_filter(
                &conn,
                filter,
                |task: &Task| task.id.as_str(),
                |task: &Task| task.status.as_str(),
                &mut tasks,
            )?;
        }
        Ok(tasks)
    }

    /// Look up a cached pane-titlebar summary for a work item.
    /// Returns `(summary, basis_hash)` so callers can compare the
    /// stored basis against a freshly computed one to decide whether
    /// the cache is still valid.
    pub fn get_pane_summary(&self, work_item_id: &str) -> Result<Option<(String, String)>> {
        let conn = self.connect()?;
        let row = conn
            .query_row(
                "SELECT summary, basis_hash FROM pane_summaries WHERE work_item_id = ?1",
                params![work_item_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// Insert or replace the cached pane summary for a work item.
    /// `basis_hash` should be derived from the inputs that, if
    /// changed, invalidate the cached summary (typically a hash of
    /// name + description).
    pub fn set_pane_summary(
        &self,
        work_item_id: &str,
        summary: &str,
        basis_hash: &str,
    ) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO pane_summaries (work_item_id, summary, basis_hash, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(work_item_id) DO UPDATE SET
                 summary = excluded.summary,
                 basis_hash = excluded.basis_hash,
                 created_at = excluded.created_at",
            params![work_item_id, summary, basis_hash, now_string()],
        )?;
        Ok(())
    }

    pub fn list_chores(
        &self,
        product_id: &str,
        dep_filter: Option<&DependencyFilter>,
    ) -> Result<Vec<Task>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority
             FROM tasks
             WHERE product_id = ?1 AND kind = 'chore' AND deleted_at IS NULL
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([product_id], map_task)?;
        let mut chores: Vec<Task> = collect_rows(rows)?;
        if let Some(filter) = dep_filter {
            apply_dep_filter(
                &conn,
                filter,
                |chore: &Task| chore.id.as_str(),
                |chore: &Task| chore.status.as_str(),
                &mut chores,
            )?;
        }
        Ok(chores)
    }

    /// Read the unsatisfied prerequisites of `work_item_id` outside
    /// of any in-flight transaction. Used by the engine app to refuse
    /// `RequestExecution` against a gated work item (see
    /// `boss::engine::app`).
    pub fn gating_prereqs_for(&self, work_item_id: &str) -> Result<Vec<String>> {
        let conn = self.connect()?;
        deps::gating_prereqs_for(&conn, work_item_id)
    }

    /// Declare a `relation` edge from `dependent` to `prerequisite`.
    /// Validates both endpoints resolve to live work items in the
    /// same product, refuses self-edges and cycles, and is
    /// idempotent on a re-add of an existing edge.
    ///
    /// v1 ships only `relation = 'blocks'`. The CLI accepts an
    /// explicit `--relation` flag but rejects anything else; the
    /// column accepts any TEXT value at the schema level so future
    /// relation types can ship without a re-migration.
    pub fn add_dependency(&self, input: AddDependencyInput) -> Result<WorkItemDependency> {
        let relation = input
            .relation
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(RELATION_BLOCKS);
        if relation != RELATION_BLOCKS {
            bail!(
                "unsupported dependency relation `{relation}`; only `blocks` is implemented in v1"
            );
        }
        let dependent_id = input.dependent.trim();
        let prerequisite_id = input.prerequisite.trim();
        if dependent_id.is_empty() || prerequisite_id.is_empty() {
            bail!("dependent and prerequisite ids are required");
        }
        if dependent_id == prerequisite_id {
            bail!("a work item cannot depend on itself: {dependent_id}");
        }

        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        // Both ids must resolve and live in the same product. Cross-
        // product edges are tracked separately (see proj_18a2bbe20fc03718_8).
        let dependent_product = product_id_for_work_item(&tx, dependent_id)?;
        let prerequisite_product = product_id_for_work_item(&tx, prerequisite_id)?;
        if dependent_product != prerequisite_product {
            bail!(
                "dependency edges must stay within a single product; cross-product edges are tracked in proj_18a2bbe20fc03718_8"
            );
        }
        if deps::would_create_cycle(&tx, dependent_id, prerequisite_id)? {
            bail!("creating this edge would form a cycle: {prerequisite_id} → … → {dependent_id}");
        }
        let now = now_string();
        let (edge, _outcome): (WorkItemDependency, EdgeInsertOutcome) =
            deps::insert_edge(&tx, dependent_id, prerequisite_id, relation, &now)?;
        // Auto-block (Q4): if the dependent isn't already `blocked`
        // and the new edge introduces a gating prereq, the engine
        // flips it to `blocked` and stamps `last_status_actor =
        // 'engine'` so the eventual auto-unblock knows the engine
        // owns this transition.
        maybe_engine_block_dependent(&tx, dependent_id, &now)?;
        tx.commit()?;
        Ok(edge)
    }

    /// Drop the named edge if it exists. No-op success when the edge
    /// is already absent (mirrors `boss <kind> delete` on an
    /// already-archived row).
    pub fn remove_dependency(&self, input: RemoveDependencyInput) -> Result<bool> {
        let relation = input
            .relation
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(RELATION_BLOCKS);
        let dependent_id = input.dependent.trim();
        let prerequisite_id = input.prerequisite.trim();
        if dependent_id.is_empty() || prerequisite_id.is_empty() {
            bail!("dependent and prerequisite ids are required");
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let removed = deps::delete_edge(&tx, dependent_id, prerequisite_id, relation)?;
        // Auto-unblock (Q4): when the only remaining gating reason is
        // gone and the engine itself put this row in `blocked`, flip
        // it back to `todo`. Manual blocks (last_status_actor =
        // 'human') stick — the user still has to clear them.
        let now = now_string();
        maybe_engine_unblock_dependent(&tx, dependent_id, &now)?;
        tx.commit()?;
        Ok(removed)
    }

    /// Return the prerequisites and/or dependents of a single work
    /// item. Empty lists when nothing matches; errors only when the
    /// work item id itself is unknown.
    pub fn list_dependencies(
        &self,
        input: ListDependenciesInput,
    ) -> Result<WorkItemDependencyView> {
        let work_item_id = input.work_item.trim();
        if work_item_id.is_empty() {
            bail!("work_item id is required");
        }
        let conn = self.connect()?;
        // Validate the work item exists by classifying its id and
        // looking it up. Surfaces a clear error rather than returning
        // an empty list for typos.
        let _ = product_id_for_work_item(&conn, work_item_id)?;

        let direction = input.direction.unwrap_or_default();
        let prerequisites = match direction {
            DependencyDirection::Dependents => Vec::new(),
            DependencyDirection::Prereqs | DependencyDirection::Both => {
                deps::prerequisites_of(&conn, work_item_id, None)?
            }
        };
        let dependents = match direction {
            DependencyDirection::Prereqs => Vec::new(),
            DependencyDirection::Dependents | DependencyDirection::Both => {
                deps::dependents_of(&conn, work_item_id, None)?
            }
        };
        Ok(WorkItemDependencyView {
            work_item_id: work_item_id.to_owned(),
            prerequisites,
            dependents,
        })
    }

    /// Resolved counterpart of [`Self::list_dependencies`]: each edge
    /// is collapsed into the peer's id + status + name + kind so the
    /// CLI / app shows the gate context without a second lookup.
    /// Drives the `boss <kind> show` Dependencies section (Q6).
    pub fn list_dependencies_detailed(
        &self,
        input: ListDependenciesInput,
    ) -> Result<WorkItemDependencyDetail> {
        let work_item_id = input.work_item.trim();
        if work_item_id.is_empty() {
            bail!("work_item id is required");
        }
        let conn = self.connect()?;
        let _ = product_id_for_work_item(&conn, work_item_id)?;

        let direction = input.direction.unwrap_or_default();
        let prerequisites = match direction {
            DependencyDirection::Dependents => Vec::new(),
            DependencyDirection::Prereqs | DependencyDirection::Both => {
                let edges = deps::prerequisites_of(&conn, work_item_id, None)?;
                edges
                    .into_iter()
                    .map(|edge| {
                        let peer_id = edge.prerequisite_id.clone();
                        resolve_dependency_edge(&conn, &peer_id, &edge.relation)
                    })
                    .collect::<Result<Vec<_>>>()?
            }
        };
        let dependents = match direction {
            DependencyDirection::Prereqs => Vec::new(),
            DependencyDirection::Dependents | DependencyDirection::Both => {
                let edges = deps::dependents_of(&conn, work_item_id, None)?;
                edges
                    .into_iter()
                    .map(|edge| {
                        let peer_id = edge.dependent_id.clone();
                        resolve_dependency_edge(&conn, &peer_id, &edge.relation)
                    })
                    .collect::<Result<Vec<_>>>()?
            }
        };
        Ok(WorkItemDependencyDetail {
            work_item_id: work_item_id.to_owned(),
            prerequisites,
            dependents,
        })
    }

    fn init(&self) -> Result<()> {
        let conn = self.connect()?;
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS metadata (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS products (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                slug TEXT NOT NULL UNIQUE,
                description TEXT NOT NULL DEFAULT '',
                repo_remote_url TEXT,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY,
                product_id TEXT NOT NULL REFERENCES products(id),
                name TEXT NOT NULL,
                slug TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                goal TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL,
                priority TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE UNIQUE INDEX IF NOT EXISTS projects_product_slug_idx
                ON projects(product_id, slug);

            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                product_id TEXT NOT NULL REFERENCES products(id),
                project_id TEXT REFERENCES projects(id),
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL,
                ordinal INTEGER,
                pr_url TEXT,
                deleted_at TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                autostart INTEGER NOT NULL DEFAULT 1,
                priority TEXT NOT NULL DEFAULT 'medium'
            );

            CREATE INDEX IF NOT EXISTS tasks_product_idx
                ON tasks(product_id, kind, deleted_at);

            CREATE INDEX IF NOT EXISTS tasks_project_idx
                ON tasks(project_id, deleted_at, ordinal);

            CREATE TABLE IF NOT EXISTS work_executions (
                id TEXT PRIMARY KEY,
                work_item_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                repo_remote_url TEXT NOT NULL,
                cube_repo_id TEXT,
                cube_lease_id TEXT,
                cube_workspace_id TEXT,
                workspace_path TEXT,
                priority INTEGER NOT NULL DEFAULT 0,
                preferred_workspace_id TEXT,
                created_at TEXT NOT NULL,
                started_at TEXT,
                finished_at TEXT
            );

            CREATE INDEX IF NOT EXISTS work_executions_work_item_idx
                ON work_executions(work_item_id, created_at);

            CREATE TABLE IF NOT EXISTS work_runs (
                id TEXT PRIMARY KEY,
                execution_id TEXT NOT NULL REFERENCES work_executions(id) ON DELETE CASCADE,
                agent_id TEXT NOT NULL,
                status TEXT NOT NULL,
                error_text TEXT,
                result_summary TEXT,
                transcript_path TEXT,
                artifacts_path TEXT,
                created_at TEXT NOT NULL,
                started_at TEXT,
                finished_at TEXT
            );

            CREATE INDEX IF NOT EXISTS work_runs_execution_idx
                ON work_runs(execution_id, created_at);

            CREATE TABLE IF NOT EXISTS work_attention_items (
                id TEXT PRIMARY KEY,
                execution_id TEXT NOT NULL REFERENCES work_executions(id) ON DELETE CASCADE,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                title TEXT NOT NULL,
                body_markdown TEXT NOT NULL,
                created_at TEXT NOT NULL,
                resolved_at TEXT
            );

            CREATE INDEX IF NOT EXISTS work_attention_items_execution_idx
                ON work_attention_items(execution_id, created_at);

            CREATE TABLE IF NOT EXISTS pane_summaries (
                work_item_id TEXT PRIMARY KEY,
                summary TEXT NOT NULL,
                basis_hash TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS work_item_dependencies (
                dependent_id     TEXT NOT NULL,
                prerequisite_id  TEXT NOT NULL,
                relation         TEXT NOT NULL DEFAULT 'blocks',
                created_at       TEXT NOT NULL,
                PRIMARY KEY (dependent_id, prerequisite_id, relation),
                CHECK (dependent_id <> prerequisite_id)
            );

            CREATE INDEX IF NOT EXISTS work_item_dependencies_prereq_idx
                ON work_item_dependencies(prerequisite_id, relation);

            CREATE INDEX IF NOT EXISTS work_item_dependencies_dependent_idx
                ON work_item_dependencies(dependent_id, relation);
            ",
        )?;
        migrate_work_executions_v3(&conn)?;
        migrate_tasks_autostart(&conn)?;
        migrate_last_status_actor(&conn)?;
        migrate_tasks_priority(&conn)?;
        migrate_backfill_project_design_tasks(&conn)?;
        // Index creation must follow migration: pre-v3 databases don't
        // have `priority` until `migrate_work_executions_v3` adds it,
        // and SQLite's `CREATE INDEX IF NOT EXISTS` errors on missing
        // columns rather than silently skipping. Keep this out of the
        // schema-init batch so a pre-v3 database can still be opened.
        conn.execute(
            "CREATE INDEX IF NOT EXISTS work_executions_ready_idx
                ON work_executions(status, priority, created_at)",
            [],
        )?;
        migrate_timestamps_to_epoch(&conn)?;
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES ('schema_version', '4')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )?;
        Ok(())
    }

    fn connect(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path)
            .with_context(|| format!("failed to open work db {}", self.path.display()))?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(conn)
    }

    fn update_product(&self, id: &str, patch: WorkItemPatch) -> Result<WorkItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut product =
            query_product(&tx, id)?.with_context(|| format!("unknown product: {id}"))?;

        apply_text_patch(&mut product.name, patch.name);
        apply_text_patch(&mut product.description, patch.description);
        apply_optional_patch(&mut product.repo_remote_url, patch.repo_remote_url);
        apply_text_patch(&mut product.status, patch.status);
        product.slug = unique_product_slug_for_update(&tx, id, &slugify(&product.name))?;
        product.updated_at = now_string();

        tx.execute(
            "UPDATE products
             SET name = ?2, slug = ?3, description = ?4, repo_remote_url = ?5, status = ?6, updated_at = ?7
             WHERE id = ?1",
            params![
                product.id,
                product.name,
                product.slug,
                product.description,
                product.repo_remote_url,
                product.status,
                product.updated_at,
            ],
        )?;

        let updated = query_product(&tx, id)?.with_context(|| format!("unknown product: {id}"))?;
        tx.commit()?;
        Ok(WorkItem::Product(updated))
    }

    fn update_project(&self, id: &str, patch: WorkItemPatch) -> Result<WorkItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut project =
            query_project(&tx, id)?.with_context(|| format!("unknown project: {id}"))?;
        let previous_status = project.status.clone();
        let status_changed = patch.status.is_some();

        apply_text_patch(&mut project.name, patch.name);
        apply_text_patch(&mut project.description, patch.description);
        apply_text_patch(&mut project.goal, patch.goal);
        apply_text_patch(&mut project.status, patch.status);
        apply_text_patch(&mut project.priority, patch.priority);
        project.slug =
            unique_project_slug_for_update(&tx, &project.product_id, id, &slugify(&project.name))?;
        project.updated_at = now_string();

        if status_changed {
            refuse_manual_move_off_blocked_while_gated(&tx, id, &previous_status, &project.status)?;
        }
        let actor = if status_changed { "human" } else { "" };

        tx.execute(
            "UPDATE projects
             SET name = ?2, slug = ?3, description = ?4, goal = ?5, status = ?6, priority = ?7, updated_at = ?8,
                 last_status_actor = CASE WHEN ?9 = '' THEN last_status_actor ELSE ?9 END
             WHERE id = ?1",
            params![
                project.id,
                project.name,
                project.slug,
                project.description,
                project.goal,
                project.status,
                project.priority,
                project.updated_at,
                actor,
            ],
        )?;

        if status_changed && previous_status != project.status {
            cascade_dependents_after_prereq_status_change(
                &tx,
                id,
                &project.status,
                &project.updated_at,
            )?;
        }

        let updated = query_project(&tx, id)?.with_context(|| format!("unknown project: {id}"))?;
        tx.commit()?;
        Ok(WorkItem::Project(updated))
    }

    fn update_task(&self, id: &str, patch: WorkItemPatch) -> Result<WorkItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut task = query_task(&tx, id)?.with_context(|| format!("unknown task: {id}"))?;
        if task.deleted_at.is_some() {
            bail!("cannot update a deleted task: {id}");
        }
        let previous_status = task.status.clone();
        let status_changed = patch.status.is_some();

        apply_text_patch(&mut task.name, patch.name);
        apply_text_patch(&mut task.description, patch.description);
        apply_text_patch(&mut task.status, patch.status);
        apply_optional_patch(&mut task.pr_url, patch.pr_url);
        if let Some(priority_patch) = patch.priority {
            task.priority = normalize_priority(Some(&priority_patch))?;
        }
        if let Some(ordinal) = patch.ordinal {
            task.ordinal = Some(ordinal);
        }
        task.updated_at = now_string();

        if status_changed {
            refuse_manual_move_off_blocked_while_gated(&tx, id, &previous_status, &task.status)?;
        }
        let actor = if status_changed { "human" } else { "" };

        tx.execute(
            "UPDATE tasks
             SET name = ?2, description = ?3, status = ?4, ordinal = ?5, pr_url = ?6, updated_at = ?7,
                 priority = ?9,
                 last_status_actor = CASE WHEN ?8 = '' THEN last_status_actor ELSE ?8 END
             WHERE id = ?1",
            params![
                task.id,
                task.name,
                task.description,
                task.status,
                task.ordinal,
                task.pr_url,
                task.updated_at,
                actor,
                task.priority,
            ],
        )?;

        if status_changed && previous_status != task.status {
            cascade_dependents_after_prereq_status_change(&tx, id, &task.status, &task.updated_at)?;
        }

        let updated = query_task(&tx, id)?.with_context(|| format!("unknown task: {id}"))?;
        tx.commit()?;
        Ok(task_to_item(updated))
    }

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
        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
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
        // keep the existing status.
        let new_status = match target {
            _ if task.status == "done" || task.status == "archived" => task.status.clone(),
            WorkerPrCompletionTarget::InReview if task.status == "in_review" => task.status.clone(),
            WorkerPrCompletionTarget::InReview => "in_review".to_owned(),
            WorkerPrCompletionTarget::Done => "done".to_owned(),
        };
        tx.execute(
            "UPDATE tasks
             SET status = ?2,
                 pr_url = ?3,
                 updated_at = ?4,
                 last_status_actor = 'engine'
             WHERE id = ?1",
            params![task.id, new_status, pr_url, now],
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
                 finished_at = ?2
             WHERE id = ?1",
            params![execution_id, now],
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

        let updated_execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
        let updated_task = query_task(&tx, &work_item_id)?
            .with_context(|| format!("unknown task: {work_item_id}"))?;
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
             WHERE kind IN ('chore', 'project_task', 'design')
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
        tx.execute(
            "UPDATE tasks
             SET status = 'done',
                 pr_url = ?2,
                 updated_at = ?3,
                 last_status_actor = 'engine'
             WHERE id = ?1",
            params![task.id, pr_url, now],
        )?;
        cascade_dependents_after_prereq_status_change(&tx, &task.id, "done", &now)?;
        let updated = query_task(&tx, work_item_id)?
            .with_context(|| format!("unknown task after update: {work_item_id}"))?;
        tx.commit()?;
        Ok(Some(updated))
    }

    /// Atomically null out `cube_lease_id`, `cube_workspace_id`, and
    /// `workspace_path` on `execution_id`. Returns the prior lease id
    /// — `Some` means the caller is responsible for issuing the cube
    /// `workspace release`, `None` means there was nothing to release
    /// (already cleared by an earlier path or never leased).
    ///
    /// Used by the engine-side release path (manual chore-done update,
    /// `bossctl agents stop`) to claim ownership of the cube release
    /// before calling out to the cube CLI, so two concurrent callers
    /// don't issue duplicate releases against the same lease.
    pub fn clear_execution_workspace(&self, execution_id: &str) -> Result<Option<String>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
        let prior = execution.cube_lease_id.clone();
        if prior.is_some() {
            tx.execute(
                "UPDATE work_executions
                 SET cube_lease_id = NULL,
                     cube_workspace_id = NULL,
                     workspace_path = NULL
                 WHERE id = ?1",
                params![execution_id],
            )?;
        }
        tx.commit()?;
        Ok(prior)
    }

    /// Most-recent execution for `work_item_id`, ordered by creation.
    /// `Ok(None)` when the work item has never had an execution.
    pub fn latest_execution_for_work_item(
        &self,
        work_item_id: &str,
    ) -> Result<Option<WorkExecution>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                    cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                    created_at, started_at, finished_at
             FROM work_executions
             WHERE work_item_id = ?1
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
        )?;
        let mut rows = stmt.query_map([work_item_id], map_execution)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }
}

/// Where the chore should land after [`WorkDb::record_worker_pr_completion`].
/// `InReview` is the typical case (open PR, ready for human review);
/// `Done` is used when the PR was already merged at the time the
/// worker's Stop event fired, so we skip the review column entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerPrCompletionTarget {
    InReview,
    Done,
}

/// Result of a successful [`WorkDb::record_worker_pr_completion`] call.
/// Carries the cube lease/workspace ids that were attached to the
/// execution so the caller can drive cube release out-of-band.
#[derive(Debug, Clone)]
pub struct WorkerPrCompletion {
    pub execution: WorkExecution,
    pub work_item: WorkItem,
    pub released_lease_id: Option<String>,
    pub released_workspace_id: Option<String>,
}

/// One row from [`WorkDb::list_chores_pending_merge_check`]: a chore
/// or project_task the merge poller still needs to ask GitHub about.
#[derive(Debug, Clone)]
pub struct PendingMergeCheck {
    pub work_item_id: String,
    pub product_id: String,
    pub pr_url: String,
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&Row<'_>) -> rusqlite::Result<T>>,
) -> Result<Vec<T>> {
    let mut values = Vec::new();
    for row in rows {
        values.push(row?);
    }
    Ok(values)
}

fn map_product(row: &Row<'_>) -> rusqlite::Result<Product> {
    Ok(Product {
        id: row.get(0)?,
        name: row.get(1)?,
        slug: row.get(2)?,
        description: row.get(3)?,
        repo_remote_url: row.get(4)?,
        status: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn map_project(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        product_id: row.get(1)?,
        name: row.get(2)?,
        slug: row.get(3)?,
        description: row.get(4)?,
        goal: row.get(5)?,
        status: row.get(6)?,
        priority: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
        last_status_actor: row.get(10)?,
    })
}

fn map_task(row: &Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get(0)?,
        product_id: row.get(1)?,
        project_id: row.get(2)?,
        kind: row.get(3)?,
        name: row.get(4)?,
        description: row.get(5)?,
        status: row.get(6)?,
        ordinal: row.get(7)?,
        pr_url: row.get(8)?,
        deleted_at: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        autostart: row.get::<_, i64>(12)? != 0,
        last_status_actor: row.get(13)?,
        priority: row.get(14)?,
    })
}

fn map_execution(row: &Row<'_>) -> rusqlite::Result<WorkExecution> {
    Ok(WorkExecution {
        id: row.get(0)?,
        work_item_id: row.get(1)?,
        kind: row.get(2)?,
        status: row.get(3)?,
        repo_remote_url: row.get(4)?,
        cube_repo_id: row.get(5)?,
        cube_lease_id: row.get(6)?,
        cube_workspace_id: row.get(7)?,
        workspace_path: row.get(8)?,
        priority: row.get(9)?,
        preferred_workspace_id: row.get(10)?,
        created_at: row.get(11)?,
        started_at: row.get(12)?,
        finished_at: row.get(13)?,
    })
}

fn map_run(row: &Row<'_>) -> rusqlite::Result<WorkRun> {
    Ok(WorkRun {
        id: row.get(0)?,
        execution_id: row.get(1)?,
        agent_id: row.get(2)?,
        status: row.get(3)?,
        error_text: row.get(4)?,
        result_summary: row.get(5)?,
        transcript_path: row.get(6)?,
        artifacts_path: row.get(7)?,
        created_at: row.get(8)?,
        started_at: row.get(9)?,
        finished_at: row.get(10)?,
    })
}

fn map_attention_item(row: &Row<'_>) -> rusqlite::Result<WorkAttentionItem> {
    Ok(WorkAttentionItem {
        id: row.get(0)?,
        execution_id: row.get(1)?,
        kind: row.get(2)?,
        status: row.get(3)?,
        title: row.get(4)?,
        body_markdown: row.get(5)?,
        created_at: row.get(6)?,
        resolved_at: row.get(7)?,
    })
}

fn insert_task_in_tx(conn: &Connection, input: CreateTaskInput) -> Result<Task> {
    ensure_product_exists(conn, &input.product_id)?;
    ensure_project_belongs_to_product(conn, &input.project_id, &input.product_id)?;

    let id = next_id("task");
    let now = now_string();
    let ordinal = next_task_ordinal(conn, &input.project_id)?;
    let description = input.description.unwrap_or_default();
    let autostart_value: i64 = if input.autostart { 1 } else { 0 };
    let priority = normalize_priority(input.priority.as_deref())?;

    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority)
         VALUES (?1, ?2, ?3, 'project_task', ?4, ?5, 'todo', ?6, NULL, NULL, ?7, ?7, ?8, ?9)",
        params![id, input.product_id, input.project_id, input.name, description, ordinal, now, autostart_value, priority],
    )?;

    query_task(conn, &id)?.with_context(|| format!("missing task after insert: {id}"))
}

fn insert_chore_in_tx(conn: &Connection, input: CreateChoreInput) -> Result<Task> {
    ensure_product_exists(conn, &input.product_id)?;

    let id = next_id("task");
    let now = now_string();
    let description = input.description.unwrap_or_default();
    let autostart_value: i64 = if input.autostart { 1 } else { 0 };
    let priority = normalize_priority(input.priority.as_deref())?;

    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority)
         VALUES (?1, ?2, NULL, 'chore', ?3, ?4, 'todo', NULL, NULL, NULL, ?5, ?5, ?6, ?7)",
        params![id, input.product_id, input.name, description, now, autostart_value, priority],
    )?;

    query_task(conn, &id)?.with_context(|| format!("missing chore after insert: {id}"))
}

/// Insert a `kind = 'design'` task as the first row under
/// `project_id`. Used by `create_project` and the migration that
/// backfills design tasks for projects predating this column. The
/// design task always has `ordinal = 0` so it sorts ahead of every
/// `project_task` (which start at `ordinal = 1`) and the dispatcher
/// picks it up first via the existing first-incomplete chain.
fn insert_design_task_for_project_in_tx(
    conn: &Connection,
    product_id: &str,
    project_id: &str,
    autostart: bool,
) -> Result<Task> {
    let id = next_id("task");
    let now = now_string();
    let autostart_value: i64 = if autostart { 1 } else { 0 };
    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority)
         VALUES (?1, ?2, ?3, 'design', 'Design', '', 'todo', 0, NULL, NULL, ?4, ?4, ?5, 'medium')",
        params![id, product_id, project_id, now, autostart_value],
    )?;
    query_task(conn, &id)?.with_context(|| format!("missing design task after insert: {id}"))
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

fn insert_execution(conn: &Connection, input: CreateExecutionInput) -> Result<WorkExecution> {
    let repo_remote_url = resolve_execution_repo_remote_url(
        conn,
        &input.work_item_id,
        normalize_optional_text(input.repo_remote_url),
    )?;
    let id = next_id("exec");
    let now = now_string();
    let status = input.status.unwrap_or_else(|| "queued".to_owned());
    let cube_repo_id = normalize_optional_text(input.cube_repo_id);
    let cube_lease_id = normalize_optional_text(input.cube_lease_id);
    let cube_workspace_id = normalize_optional_text(input.cube_workspace_id);
    let workspace_path = normalize_optional_text(input.workspace_path);
    let priority = input.priority.unwrap_or(0);
    let preferred_workspace_id = normalize_optional_text(input.preferred_workspace_id);
    let started_at = normalize_optional_text(input.started_at);
    let finished_at = normalize_optional_text(input.finished_at);

    conn.execute(
        "INSERT INTO work_executions (
            id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
            cube_workspace_id, workspace_path, priority, preferred_workspace_id,
            created_at, started_at, finished_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            id,
            input.work_item_id,
            input.kind,
            status,
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
        ],
    )?;

    query_execution(conn, &id)?.with_context(|| format!("missing execution after insert: {id}"))
}

fn query_product(conn: &Connection, id: &str) -> Result<Option<Product>> {
    conn.query_row(
        "SELECT id, name, slug, description, repo_remote_url, status, created_at, updated_at
         FROM products
         WHERE id = ?1",
        [id],
        map_product,
    )
    .optional()
    .map_err(Into::into)
}

fn query_project(conn: &Connection, id: &str) -> Result<Option<Project>> {
    conn.query_row(
        "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at, last_status_actor
         FROM projects
         WHERE id = ?1",
        [id],
        map_project,
    )
    .optional()
    .map_err(Into::into)
}

fn query_task(conn: &Connection, id: &str) -> Result<Option<Task>> {
    conn.query_row(
        "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority
         FROM tasks
         WHERE id = ?1",
        [id],
        map_task,
    )
    .optional()
    .map_err(Into::into)
}

fn query_execution(conn: &Connection, id: &str) -> Result<Option<WorkExecution>> {
    conn.query_row(
        "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                created_at, started_at, finished_at
         FROM work_executions
         WHERE id = ?1",
        [id],
        map_execution,
    )
    .optional()
    .map_err(Into::into)
}

fn query_run(conn: &Connection, id: &str) -> Result<Option<WorkRun>> {
    conn.query_row(
        "SELECT id, execution_id, agent_id, status, error_text, result_summary, transcript_path,
                artifacts_path, created_at, started_at, finished_at
         FROM work_runs
         WHERE id = ?1",
        [id],
        map_run,
    )
    .optional()
    .map_err(Into::into)
}

fn query_attention_item(conn: &Connection, id: &str) -> Result<Option<WorkAttentionItem>> {
    conn.query_row(
        "SELECT id, execution_id, kind, status, title, body_markdown, created_at, resolved_at
         FROM work_attention_items
         WHERE id = ?1",
        [id],
        map_attention_item,
    )
    .optional()
    .map_err(Into::into)
}

fn list_projects_for_product(conn: &Connection, product_id: &str) -> Result<Vec<Project>> {
    let mut stmt = conn.prepare(
        "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at, last_status_actor
         FROM projects
         WHERE product_id = ?1
         ORDER BY created_at ASC, name COLLATE NOCASE ASC",
    )?;
    let rows = stmt.query_map([product_id], map_project)?;
    collect_rows(rows)
}

fn list_tasks_for_product(conn: &Connection, product_id: &str) -> Result<Vec<Task>> {
    let mut stmt = conn.prepare(
        "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority
         FROM tasks
         WHERE product_id = ?1 AND deleted_at IS NULL
         ORDER BY project_id ASC, ordinal ASC, created_at ASC, id ASC",
    )?;
    let rows = stmt.query_map([product_id], map_task)?;
    collect_rows(rows)
}

fn ensure_product_exists(conn: &Connection, product_id: &str) -> Result<()> {
    let exists = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM products WHERE id = ?1)",
        [product_id],
        |row| row.get::<_, i64>(0),
    )?;
    if exists == 0 {
        bail!("unknown product: {product_id}");
    }
    Ok(())
}

fn ensure_project_exists(conn: &Connection, project_id: &str) -> Result<()> {
    let exists = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1)",
        [project_id],
        |row| row.get::<_, i64>(0),
    )?;
    if exists == 0 {
        bail!("unknown project: {project_id}");
    }
    Ok(())
}

fn ensure_project_belongs_to_product(
    conn: &Connection,
    project_id: &str,
    product_id: &str,
) -> Result<()> {
    let exists = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1 AND product_id = ?2)",
        params![project_id, product_id],
        |row| row.get::<_, i64>(0),
    )?;
    if exists == 0 {
        bail!("project {project_id} does not belong to product {product_id}");
    }
    Ok(())
}

fn migrate_work_executions_v3(conn: &Connection) -> Result<()> {
    for (column, ddl) in [
        (
            "cube_workspace_id",
            "ALTER TABLE work_executions ADD COLUMN cube_workspace_id TEXT",
        ),
        (
            "priority",
            "ALTER TABLE work_executions ADD COLUMN priority INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "preferred_workspace_id",
            "ALTER TABLE work_executions ADD COLUMN preferred_workspace_id TEXT",
        ),
    ] {
        if !work_executions_has_column(conn, column)? {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
}

/// Canonicalize all timestamp columns to Unix epoch seconds (decimal
/// string). Older rows in some databases hold ISO 8601 strings (e.g.
/// `2026-05-07T18:55:45.000Z`) from a pre-canonical write path; this
/// rewrites them in-place so consumers — `boss chore list --json`,
/// the macOS app's Done-lane bucketing, and any future SQL ordering —
/// see one shape. Idempotent: rows already in epoch form are skipped
/// by the LIKE filter.
fn migrate_timestamps_to_epoch(conn: &Connection) -> Result<()> {
    const TIMESTAMP_COLUMNS: &[(&str, &str)] = &[
        ("products", "created_at"),
        ("products", "updated_at"),
        ("projects", "created_at"),
        ("projects", "updated_at"),
        ("tasks", "created_at"),
        ("tasks", "updated_at"),
        ("tasks", "deleted_at"),
        ("work_executions", "created_at"),
        ("work_executions", "started_at"),
        ("work_executions", "finished_at"),
        ("work_runs", "created_at"),
        ("work_runs", "started_at"),
        ("work_runs", "finished_at"),
        ("work_attention_items", "created_at"),
        ("work_attention_items", "resolved_at"),
        ("pane_summaries", "created_at"),
    ];
    for (table, column) in TIMESTAMP_COLUMNS {
        // SQLite LIKE: `_` matches any single character, so this picks
        // up `YYYY-MM-DD`-prefixed values without parsing every row.
        let select_sql = format!(
            "SELECT rowid, {column} FROM {table} \
             WHERE {column} LIKE '____-__-__T%' OR {column} LIKE '____-__-__ %'"
        );
        let mut stmt = conn.prepare(&select_sql)?;
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for (rowid, value) in rows {
            if let Some(epoch) = parse_iso8601_to_epoch(&value) {
                let update_sql = format!("UPDATE {table} SET {column} = ?1 WHERE rowid = ?2");
                conn.execute(&update_sql, params![epoch.to_string(), rowid])?;
            }
        }
    }
    Ok(())
}

/// Parse an ISO 8601 / RFC 3339 UTC timestamp like
/// `YYYY-MM-DDTHH:MM:SS[.fff]Z` into Unix epoch seconds. Returns
/// `None` for any other shape (already-canonical numeric strings,
/// non-UTC offsets, malformed values) so the caller can leave them
/// alone.
fn parse_iso8601_to_epoch(value: &str) -> Option<i64> {
    let s = value.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || (bytes[10] != b'T' && bytes[10] != b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    if !s.ends_with('Z') {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: u32 = s.get(11..13)?.parse().ok()?;
    let minute: u32 = s.get(14..16)?.parse().ok()?;
    let second: u32 = s.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour >= 24 || minute >= 60 || second >= 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour as i64 * 3_600 + minute as i64 * 60 + second as i64)
}

/// Days from the Unix epoch (1970-01-01) for a (year, month, day)
/// triple. Howard Hinnant's `days_from_civil`; see
/// https://howardhinnant.github.io/date_algorithms.html.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let m = month as i64;
    let y = if m <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + day as u64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe as i64 - 719_468
}

fn work_executions_has_column(conn: &Connection, column: &str) -> Result<bool> {
    table_has_column(conn, "work_executions", column)
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Add the `autostart` column to `tasks` for older databases. New
/// chores opt out of auto-dispatch by setting this column to 0;
/// `task_accepts_execution` then keeps them out of the reconcile loop
/// while their status is `todo`. Older rows default to 1 so the
/// historical "create-and-dispatch" behaviour is preserved.
fn migrate_tasks_autostart(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "tasks", "autostart")? {
        conn.execute(
            "ALTER TABLE tasks ADD COLUMN autostart INTEGER NOT NULL DEFAULT 1",
            [],
        )?;
    }
    Ok(())
}

/// Add `last_status_actor` to `tasks` and `projects` so the engine
/// can distinguish a status it set itself (`'engine'`) from one a
/// human typed at the CLI / kanban (`'human'`). The dependencies
/// auto-unblock path only flips a `blocked` row back to `todo` when
/// the engine put it there; manual blocks stay until the human
/// clears them. Existing rows default to `'human'` so legacy blocks
/// keep manual semantics across the upgrade.
fn migrate_last_status_actor(conn: &Connection) -> Result<()> {
    for table in ["tasks", "projects"] {
        if !table_has_column(conn, table, "last_status_actor")? {
            let ddl = format!(
                "ALTER TABLE {table} ADD COLUMN last_status_actor TEXT NOT NULL DEFAULT 'human'"
            );
            conn.execute(&ddl, [])?;
        }
    }
    Ok(())
}

/// Add `priority` to `tasks` so chores and project_tasks have the
/// same first-class priority field that `projects` already had.
/// Existing rows default to `medium`. The vocabulary mirrors
/// `projects.priority` exactly (`low` / `medium` / `high`) so kanban
/// surfaces can render every work-item kind with one chip palette.
fn migrate_tasks_priority(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "tasks", "priority")? {
        conn.execute(
            "ALTER TABLE tasks ADD COLUMN priority TEXT NOT NULL DEFAULT 'medium'",
            [],
        )?;
    }
    Ok(())
}

/// Backfill a `kind = 'design'` task for every project that doesn't
/// have one yet. Brings databases that predate
/// design-as-task up to the new shape so the kanban renders them
/// like new projects: a "Design" card sits at the head of the
/// project's task list and the existing dispatcher picks it up the
/// next time `reconcile_product_executions` runs.
///
/// The backfilled design task lands in `todo` with `autostart = 0`.
/// Why parked-by-default: an existing project that's already been
/// designed (or is mid-flight under the old project-id-keyed
/// project_design execution) shouldn't get a duplicate worker
/// spawned out from under the user. A human who actually wants the
/// new design task to run can flip it to active in the kanban — the
/// same path any other parked task takes — and the autostart gate
/// melts away on first move-off-`todo`.
fn migrate_backfill_project_design_tasks(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.product_id
         FROM projects p
         WHERE NOT EXISTS (
             SELECT 1 FROM tasks t
             WHERE t.project_id = p.id
               AND t.kind = 'design'
               AND t.deleted_at IS NULL
         )",
    )?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    for (project_id, product_id) in rows {
        let id = next_id("task");
        let now = now_string();
        conn.execute(
            "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority)
             VALUES (?1, ?2, ?3, 'design', 'Design', '', 'todo', 0, NULL, NULL, ?4, ?4, 0, 'medium')",
            params![id, product_id, project_id, now],
        )?;
    }
    Ok(())
}

fn ensure_execution_exists(conn: &Connection, execution_id: &str) -> Result<()> {
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
fn collect_product_dependencies(
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

fn collect_task_runtimes(
    conn: &Connection,
    tasks: &[Task],
    chores: &[Task],
) -> Result<Vec<TaskRuntime>> {
    let mut runtimes = Vec::with_capacity(tasks.len() + chores.len());
    for task in tasks.iter().chain(chores.iter()) {
        let execution = query_latest_execution_for_work_item(conn, &task.id)?;
        let (execution_status, run_status, execution_id) = if let Some(execution) = execution {
            let run_status = query_latest_run_status(conn, &execution.id)?;
            (Some(execution.status), run_status, Some(execution.id))
        } else {
            (None, None, None)
        };
        runtimes.push(TaskRuntime {
            work_item_id: task.id.clone(),
            execution_status,
            run_status,
            execution_id,
        });
    }
    Ok(runtimes)
}

fn query_latest_run_status(conn: &Connection, execution_id: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT status
         FROM work_runs
         WHERE execution_id = ?1
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
        [execution_id],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(Into::into)
}

fn query_latest_execution_for_work_item(
    conn: &Connection,
    work_item_id: &str,
) -> Result<Option<WorkExecution>> {
    conn.query_row(
        "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                created_at, started_at, finished_at
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

fn reconcile_work_item_execution(
    conn: &Connection,
    result: &mut ExecutionReconcileResult,
    work_item_id: &str,
    kind: &str,
    desired_status: &str,
    repo_remote_url: Option<&str>,
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
            let Some(repo_remote_url) = repo_remote_url else {
                return Ok(());
            };
            let created = insert_execution(
                conn,
                CreateExecutionInput {
                    work_item_id: work_item_id.to_owned(),
                    kind: kind.to_owned(),
                    status: Some(effective_status.to_owned()),
                    repo_remote_url: Some(repo_remote_url.to_owned()),
                    cube_repo_id: None,
                    cube_lease_id: None,
                    cube_workspace_id: None,
                    workspace_path: None,
                    priority: None,
                    preferred_workspace_id: None,
                    started_at: None,
                    finished_at: None,
                },
            )?;
            result.created.push(created);
        }
    }

    Ok(())
}

fn request_execution_in_tx_with_live_check<F: FnOnce(&str) -> bool>(
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

    if let Some(existing) = query_latest_execution_for_work_item(conn, &work_item_id)? {
        if !execution_status_is_terminal(&existing.status) {
            // Existing non-terminal row. Two cases:
            //   - is_live=true: a worker is genuinely attached to the
            //     slot. Keep the row, refresh priority / preferred
            //     workspace, return the same execution. (Idempotent —
            //     this is what bossctl `work start` and a kanban
            //     drag both depend on for "don't double-spawn.")
            //   - is_live=false: the row is stale (waiting_human
            //     leftover from a worker that died with the app, or a
            //     run that exited without us seeing the SessionEnd
            //     hook). Mark it abandoned so future scans don't
            //     trip on it again, then fall through to insert a
            //     fresh ready row. This is what makes the kanban
            //     re-dispatch path work after a crash.
            if is_live(&existing.id) {
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
            } else {
                let now = now_string();
                conn.execute(
                    "UPDATE work_executions
                     SET status = 'abandoned',
                         finished_at = COALESCE(finished_at, ?2)
                     WHERE id = ?1",
                    params![existing.id, now],
                )?;
            }
        }
    }

    let _ = product_id_for_work_item(conn, &work_item_id)?;
    insert_execution(
        conn,
        CreateExecutionInput {
            work_item_id,
            kind,
            status: Some("ready".to_owned()),
            repo_remote_url: None,
            cube_repo_id: None,
            cube_lease_id: None,
            cube_workspace_id: None,
            workspace_path: None,
            priority,
            preferred_workspace_id,
            started_at: None,
            finished_at: None,
        },
    )
}

fn execution_kind_for_work_item(conn: &Connection, work_item_id: &str) -> Result<String> {
    Ok(match classify_id(work_item_id)? {
        ItemKind::Product => "product_design".to_owned(),
        // Project ids no longer host their own executions — the
        // project's design phase lives on its auto-created
        // `kind = 'design'` task. We keep this arm returning
        // `project_design` so legacy callers passing a project id to
        // `RequestExecution` still get a sensible execution kind for
        // logging, but the dispatch loop never actually creates
        // executions against project ids any more.
        ItemKind::Project => "project_design".to_owned(),
        ItemKind::Task => {
            let task = query_task(conn, work_item_id)?
                .filter(|task| task.deleted_at.is_none())
                .with_context(|| format!("unknown task: {work_item_id}"))?;
            match task.kind.as_str() {
                "chore" => "chore_implementation".to_owned(),
                "design" => "project_design".to_owned(),
                _ => "task_implementation".to_owned(),
            }
        }
    })
}

fn update_execution_status(
    conn: &Connection,
    execution_id: &str,
    status: &str,
) -> Result<WorkExecution> {
    let updated = conn.execute(
        "UPDATE work_executions SET status = ?2 WHERE id = ?1",
        params![execution_id, status],
    )?;
    if updated == 0 {
        bail!("unknown execution: {execution_id}");
    }

    query_execution(conn, execution_id)?
        .with_context(|| format!("unknown execution: {execution_id}"))
}

fn can_reconcile_execution_status(status: &str) -> bool {
    matches!(status, "queued" | "ready" | "waiting_dependency")
}

fn execution_status_is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "abandoned" | "cancelled")
}

fn task_accepts_execution(task: &Task) -> bool {
    if task.deleted_at.is_some() || task.status == "done" {
        return false;
    }
    // Honour the per-task autostart opt-out while the chore/task is
    // still parked in `todo`. Once a human (or `bossctl work start`)
    // moves it to a non-`todo` status, reconcile resumes normal
    // behaviour and creates the `ready` execution. The autostart flag
    // is a one-way pause for the auto-dispatcher only — explicit
    // RequestExecution still creates a ready execution.
    if !task.autostart && task.status == "todo" {
        return false;
    }
    true
}

fn product_id_for_work_item(conn: &Connection, work_item_id: &str) -> Result<String> {
    match classify_id(work_item_id)? {
        ItemKind::Product => query_product(conn, work_item_id)?
            .map(|product| product.id)
            .with_context(|| format!("unknown product: {work_item_id}")),
        ItemKind::Project => query_project(conn, work_item_id)?
            .map(|project| project.product_id)
            .with_context(|| format!("unknown project: {work_item_id}")),
        ItemKind::Task => query_task(conn, work_item_id)?
            .filter(|task| task.deleted_at.is_none())
            .map(|task| task.product_id)
            .with_context(|| format!("unknown task: {work_item_id}")),
    }
}

fn resolve_execution_repo_remote_url(
    conn: &Connection,
    work_item_id: &str,
    explicit_repo_remote_url: Option<String>,
) -> Result<String> {
    if let Some(repo_remote_url) = explicit_repo_remote_url {
        let _ = product_id_for_work_item(conn, work_item_id)?;
        return Ok(repo_remote_url);
    }

    let product_id = product_id_for_work_item(conn, work_item_id)?;
    let product = query_product(conn, &product_id)?
        .with_context(|| format!("unknown product: {product_id}"))?;
    product.repo_remote_url.with_context(|| {
        format!(
            "work item {work_item_id} does not resolve to a product repo_remote_url; provide one explicitly"
        )
    })
}

fn next_task_ordinal(conn: &Connection, project_id: &str) -> Result<i64> {
    let current = conn.query_row(
        "SELECT COALESCE(MAX(ordinal), 0) FROM tasks
             WHERE project_id = ?1 AND kind = 'project_task' AND deleted_at IS NULL",
        [project_id],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(current + 1)
}

fn unique_product_slug(conn: &Connection, base_slug: &str) -> Result<String> {
    let base_slug = default_slug(base_slug);
    let mut candidate = base_slug.clone();
    let mut suffix = 2;
    while conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM products WHERE slug = ?1)",
        [candidate.as_str()],
        |row| row.get::<_, i64>(0),
    )? != 0
    {
        candidate = format!("{base_slug}-{suffix}");
        suffix += 1;
    }
    Ok(candidate)
}

fn unique_product_slug_for_update(conn: &Connection, id: &str, base_slug: &str) -> Result<String> {
    let base_slug = default_slug(base_slug);
    let mut candidate = base_slug.clone();
    let mut suffix = 2;
    while conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM products WHERE slug = ?1 AND id != ?2)",
        params![candidate, id],
        |row| row.get::<_, i64>(0),
    )? != 0
    {
        candidate = format!("{base_slug}-{suffix}");
        suffix += 1;
    }
    Ok(candidate)
}

fn unique_project_slug(conn: &Connection, product_id: &str, base_slug: &str) -> Result<String> {
    let base_slug = default_slug(base_slug);
    let mut candidate = base_slug.clone();
    let mut suffix = 2;
    while conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE product_id = ?1 AND slug = ?2)",
        params![product_id, candidate],
        |row| row.get::<_, i64>(0),
    )? != 0
    {
        candidate = format!("{base_slug}-{suffix}");
        suffix += 1;
    }
    Ok(candidate)
}

fn unique_project_slug_for_update(
    conn: &Connection,
    product_id: &str,
    id: &str,
    base_slug: &str,
) -> Result<String> {
    let base_slug = default_slug(base_slug);
    let mut candidate = base_slug.clone();
    let mut suffix = 2;
    while conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE product_id = ?1 AND slug = ?2 AND id != ?3)",
        params![product_id, candidate, id],
        |row| row.get::<_, i64>(0),
    )? != 0
    {
        candidate = format!("{base_slug}-{suffix}");
        suffix += 1;
    }
    Ok(candidate)
}

fn default_slug(base_slug: &str) -> String {
    if base_slug.is_empty() {
        "item".to_owned()
    } else {
        base_slug.to_owned()
    }
}

fn next_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{nanos:x}_{counter:x}")
}

fn now_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value.and_then(|text| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

fn apply_text_patch(target: &mut String, patch: Option<String>) {
    if let Some(value) = patch {
        *target = value;
    }
}

fn apply_optional_patch(target: &mut Option<String>, patch: Option<String>) {
    if let Some(value) = patch {
        *target = normalize_optional_text(Some(value));
    }
}

fn task_to_item(task: Task) -> WorkItem {
    if task.kind == "chore" {
        WorkItem::Chore(task)
    } else {
        WorkItem::Task(task)
    }
}

fn slugify(name: &str) -> String {
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

enum ItemKind {
    Product,
    Project,
    Task,
}

fn classify_id(id: &str) -> Result<ItemKind> {
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
fn resolve_dependency_edge(
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
    } else if peer_id.starts_with("task_") {
        if let Some(task) = query_task(conn, peer_id)? {
            let kind = match task.kind.as_str() {
                "chore" => "chore",
                _ => "task",
            };
            return Ok(DependencyEdge {
                id: task.id,
                relation: relation.to_owned(),
                kind: kind.to_owned(),
                name: task.name,
                status: task.status,
            });
        }
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
fn apply_dep_filter<T, F, G>(
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
fn compute_gated_work_item_ids(conn: &Connection) -> Result<HashSet<String>> {
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
/// Used by `add_dependency` (an edge that introduces a gating
/// prereq) and by status cascades on a *prereq* moving away from a
/// satisfied status.
fn maybe_engine_block_dependent(
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
    Ok(())
}

/// Flip a dependent off `blocked` if (a) its current status is
/// `blocked`, (b) `last_status_actor = 'engine'` (the engine put it
/// there), and (c) no gating prereqs remain. The unblocked status
/// is `todo` — the design's recommendation in Q4. Items that were
/// manually blocked by a human are left alone.
fn maybe_engine_unblock_dependent(
    conn: &Connection,
    dependent_id: &str,
    now_epoch: &str,
) -> Result<()> {
    let current = match deps::lookup_work_item_status(conn, dependent_id)? {
        Some(s) => s,
        None => return Ok(()),
    };
    if current != "blocked" {
        return Ok(());
    }
    let actor = lookup_last_status_actor(conn, dependent_id)?;
    if actor.as_deref() != Some("engine") {
        return Ok(());
    }
    let gating = deps::gating_prereqs_for(conn, dependent_id)?;
    if !gating.is_empty() {
        return Ok(());
    }
    write_engine_status(conn, dependent_id, "todo", now_epoch)?;
    Ok(())
}

/// Walk every dependent of `prereq_id` and apply the appropriate
/// auto-{block,unblock} cascade, depending on whether the prereq's
/// new status is satisfying or not. Used after a status change on
/// a prereq.
fn cascade_dependents_after_prereq_status_change(
    conn: &Connection,
    prereq_id: &str,
    new_prereq_status: &str,
    now_epoch: &str,
) -> Result<()> {
    let dependents = deps::dependents_of(conn, prereq_id, Some("blocks"))?;
    let satisfying = deps::status_satisfies(prereq_id, new_prereq_status);
    for edge in dependents {
        if satisfying {
            maybe_engine_unblock_dependent(conn, &edge.dependent_id, now_epoch)?;
        } else {
            maybe_engine_block_dependent(conn, &edge.dependent_id, now_epoch)?;
        }
    }
    Ok(())
}

/// Internal write that stamps `last_status_actor = 'engine'` on the
/// row. Used by the auto-block / unblock paths. Returns the new
/// status.
fn write_engine_status(
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
fn refuse_manual_move_off_blocked_while_gated(
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

fn lookup_last_status_actor(conn: &Connection, work_item_id: &str) -> Result<Option<String>> {
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

    fn temp_db_path(label: &str) -> PathBuf {
        let file = format!("boss-{label}-{}.sqlite3", next_id("test"));
        std::env::temp_dir().join(file)
    }

    /// Project creation auto-spawns a `kind = 'design'` task, which
    /// otherwise sits at the head of the project's task chain and
    /// holds the dispatcher's `ready` slot. Most legacy tests pre-date
    /// the design task and want to test the project_task ordering in
    /// isolation, so they call this helper to mark the design as
    /// already done — the rest of the chain then behaves exactly as it
    /// did before.
    fn complete_design_for_project(db: &WorkDb, project_id: &str) {
        let project = db.get_project(project_id).unwrap();
        let tasks = db.list_tasks(&project.product_id, Some(project_id), None).unwrap();
        let design = tasks
            .iter()
            .find(|t| t.kind == "design")
            .expect("project should have an auto-created design task");
        db.update_work_item(
            &design.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn creates_tree_and_soft_deletes_chores() {
        let path = temp_db_path("tree");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: Some("desc".to_owned()),
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Work taxonomy".to_owned(),
                description: None,
                goal: Some("goal".to_owned()),
                autostart: true,
            })
            .unwrap();
        let task = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "Backend schema".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();

        let tree = db.get_work_tree(&product.id).unwrap();
        assert_eq!(tree.projects.len(), 1);
        // Each project carries an auto-created `kind = 'design'` task
        // at `ordinal = 0` plus the user-created task — so the tree
        // sees both. The design task always sorts first.
        assert_eq!(tree.tasks.len(), 2);
        assert_eq!(tree.tasks[0].kind, "design");
        assert_eq!(tree.tasks[1].id, task.id);
        assert_eq!(tree.chores.len(), 1);
        assert_eq!(tree.chores[0].id, chore.id);

        db.delete_work_item(&chore.id).unwrap();
        let tree = db.get_work_tree(&product.id).unwrap();
        assert!(tree.chores.is_empty());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn create_many_tasks_inserts_all_in_one_transaction() {
        let path = temp_db_path("create-many-tasks");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Plan".to_owned(),
                description: None,
                goal: None,
                autostart: true,
            })
            .unwrap();

        let inputs = (0..5)
            .map(|i| CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: format!("Task {i}"),
                description: Some(format!("d{i}")),
                autostart: i % 2 == 0,
                priority: None,
            })
            .collect::<Vec<_>>();
        let created = db
            .create_many_tasks(CreateManyTasksInput { items: inputs })
            .unwrap();

        assert_eq!(created.len(), 5);
        // ordinals must be contiguous 1..=5 — the in-tx
        // next_task_ordinal call has to see prior inserts.
        let mut ords: Vec<i64> = created.iter().map(|t| t.ordinal.unwrap()).collect();
        ords.sort();
        assert_eq!(ords, vec![1, 2, 3, 4, 5]);
        for (i, task) in created.iter().enumerate() {
            assert_eq!(task.name, format!("Task {i}"));
            assert_eq!(task.autostart, i % 2 == 0);
        }

        let tasks = db
            .list_tasks(&product.id, Some(&project.id), None)
            .unwrap();
        // Five user-created tasks plus the auto-created design task
        // that every new project carries.
        assert_eq!(tasks.len(), 6);
        assert!(tasks.iter().any(|t| t.kind == "design"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn create_many_tasks_rolls_back_on_invalid_item() {
        let path = temp_db_path("create-many-tasks-rollback");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Plan".to_owned(),
                description: None,
                goal: None,
                autostart: true,
            })
            .unwrap();

        // Item 1 references a non-existent project. The whole batch
        // must roll back — no rows visible after the failure.
        let inputs = vec![
            CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "Good".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            },
            CreateTaskInput {
                product_id: product.id.clone(),
                project_id: "proj_does_not_exist".to_owned(),
                name: "Bad".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            },
        ];
        let err = db
            .create_many_tasks(CreateManyTasksInput { items: inputs })
            .expect_err("expected rollback");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("item 1"),
            "error must name failing index: {msg}"
        );

        let tasks = db
            .list_tasks(&product.id, Some(&project.id), None)
            .unwrap();
        // The batch's project_task inserts must roll back, but the
        // auto-created design task (inserted in `create_project`'s
        // own committed transaction) is not part of this batch and
        // remains. Assert exactly that shape so a future regression
        // that lets the Bad row leak out shows up.
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].kind, "design");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn create_many_chores_inserts_all_atomically() {
        let path = temp_db_path("create-many-chores");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();

        let inputs = (0..3)
            .map(|i| CreateChoreInput {
                product_id: product.id.clone(),
                name: format!("Chore {i}"),
                description: None,
                autostart: false,
                priority: None,
            })
            .collect::<Vec<_>>();
        let created = db
            .create_many_chores(CreateManyChoresInput { items: inputs })
            .unwrap();
        assert_eq!(created.len(), 3);
        for chore in &created {
            assert_eq!(chore.kind, "chore");
            assert!(!chore.autostart);
        }

        let _ = std::fs::remove_file(path);
    }

    /// `get_work_tree` should return a `task_runtimes` entry for every
    /// active task and chore, so the kanban can render an activity icon
    /// per Doing-lane card. Tasks with no execution carry `None` for
    /// both status fields; tasks mid-run carry the live execution +
    /// run statuses.
    #[test]
    fn work_tree_includes_runtime_status_per_task() {
        let path = temp_db_path("runtime");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore_idle = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Idle".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let chore_running = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Running".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();

        // Drive the second chore's execution into a running run.
        let running_execution = db
            .list_executions(Some(&chore_running.id))
            .unwrap()
            .pop()
            .unwrap();
        db.start_execution_run(
            &running_execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();

        let tree = db.get_work_tree(&product.id).unwrap();
        let runtime_idle = tree
            .task_runtimes
            .iter()
            .find(|r| r.work_item_id == chore_idle.id)
            .expect("missing idle runtime entry");
        // The reconcile creates a `ready` execution before any run.
        assert_eq!(runtime_idle.execution_status.as_deref(), Some("ready"));
        assert_eq!(runtime_idle.run_status, None);

        let runtime_running = tree
            .task_runtimes
            .iter()
            .find(|r| r.work_item_id == chore_running.id)
            .expect("missing running runtime entry");
        assert_eq!(runtime_running.execution_status.as_deref(), Some("running"));
        assert_eq!(runtime_running.run_status.as_deref(), Some("active"));

        let _ = std::fs::remove_file(path);
    }

    /// `get_work_tree` should ship the product's `work_item_dependencies`
    /// edges so the kanban can render "Blocked by <prereq title>" on
    /// gated cards without an extra round trip. The query is scoped
    /// to the product (cross-product edges and edges referencing
    /// soft-deleted dependents must not leak).
    #[test]
    fn work_tree_includes_product_scoped_dependency_edges() {
        let path = temp_db_path("deps");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let prereq = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Prereq".to_owned(),
                description: None,
                autostart: false,
                priority: None,
            })
            .unwrap();
        let dependent = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Dependent".to_owned(),
                description: None,
                autostart: false,
                priority: None,
            })
            .unwrap();
        db.add_dependency(AddDependencyInput {
            dependent: dependent.id.clone(),
            prerequisite: prereq.id.clone(),
            relation: None,
        })
        .unwrap();

        // A second product with its own edge — must NOT leak into the
        // first product's tree.
        let other_product = db
            .create_product(CreateProductInput {
                name: "Other".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/other.git".to_owned()),
            })
            .unwrap();
        let other_prereq = db
            .create_chore(CreateChoreInput {
                product_id: other_product.id.clone(),
                name: "Other Prereq".to_owned(),
                description: None,
                autostart: false,
                priority: None,
            })
            .unwrap();
        let other_dependent = db
            .create_chore(CreateChoreInput {
                product_id: other_product.id.clone(),
                name: "Other Dependent".to_owned(),
                description: None,
                autostart: false,
                priority: None,
            })
            .unwrap();
        db.add_dependency(AddDependencyInput {
            dependent: other_dependent.id.clone(),
            prerequisite: other_prereq.id.clone(),
            relation: None,
        })
        .unwrap();

        let tree = db.get_work_tree(&product.id).unwrap();
        assert_eq!(tree.dependencies.len(), 1, "{:?}", tree.dependencies);
        let edge = &tree.dependencies[0];
        assert_eq!(edge.dependent_id, dependent.id);
        assert_eq!(edge.prerequisite_id, prereq.id);
        assert_eq!(edge.relation, "blocks");

        let other_tree = db.get_work_tree(&other_product.id).unwrap();
        assert_eq!(other_tree.dependencies.len(), 1);
        assert_eq!(other_tree.dependencies[0].dependent_id, other_dependent.id);

        let _ = std::fs::remove_file(path);
    }

    /// A pre-v3 database has `work_executions` without `priority`,
    /// `cube_workspace_id`, or `preferred_workspace_id`. Opening the
    /// db must apply the column migrations and the `priority`-keyed
    /// index without erroring.
    #[test]
    fn opens_pre_v3_database_without_priority_column() {
        let path = temp_db_path("pre-v3");
        // Build a minimal pre-v3 schema: just the table the migration
        // touches, missing the three v3 columns.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE work_executions (
                id TEXT PRIMARY KEY,
                work_item_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                repo_remote_url TEXT NOT NULL,
                cube_repo_id TEXT,
                cube_lease_id TEXT,
                workspace_path TEXT,
                created_at TEXT NOT NULL,
                started_at TEXT,
                finished_at TEXT
            );",
        )
        .unwrap();
        drop(conn);

        // This used to fail with `no such column: priority` because the
        // index DDL was in the same batch as the table DDL, so the
        // migration that adds the column never got a chance to run.
        let db = WorkDb::open(path.clone()).unwrap();

        // Sanity-check that v3 columns are now present and the index
        // exists.
        let conn = db.connect().unwrap();
        let cols: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(work_executions)").unwrap();
            stmt.query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .filter_map(Result::ok)
                .collect()
        };
        assert!(cols.contains(&"priority".to_owned()));
        assert!(cols.contains(&"cube_workspace_id".to_owned()));
        assert!(cols.contains(&"preferred_workspace_id".to_owned()));

        let index_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' AND name = 'work_executions_ready_idx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_exists, 1);

        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reorders_project_tasks() {
        let path = temp_db_path("reorder");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Taxonomy".to_owned(),
                description: None,
                goal: None,
                autostart: true,
            })
            .unwrap();
        let first = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "One".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let second = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "Two".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();

        db.reorder_project_tasks(&project.id, &[second.id.clone(), first.id.clone()])
            .unwrap();

        // The design task always sits at `ordinal = 0`, so it stays
        // at index 0 regardless of how the user-created project_tasks
        // are reordered. The reorder swap applies to the project_task
        // pair only, which now occupy indices 1 and 2.
        let tree = db.get_work_tree(&product.id).unwrap();
        assert_eq!(tree.tasks[0].kind, "design");
        assert_eq!(tree.tasks[1].id, second.id);
        assert_eq!(tree.tasks[2].id, first.id);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn creates_and_lists_execution_entities() {
        let path = temp_db_path("executions");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Execution foundation".to_owned(),
                description: None,
                goal: None,
                autostart: true,
            })
            .unwrap();
        let task = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "Schema".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();

        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: task.id.clone(),
                kind: "task_implementation".to_owned(),
                status: Some("ready".to_owned()),
                repo_remote_url: None,
                cube_repo_id: Some("cube_repo_mono".to_owned()),
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: Some("/tmp/mono-agent-001".to_owned()),
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();
        assert_eq!(
            execution.repo_remote_url,
            "git@github.com:spinyfin/mono.git"
        );

        let run = db
            .create_run(CreateRunInput {
                execution_id: execution.id.clone(),
                agent_id: "agent_1".to_owned(),
                status: Some("active".to_owned()),
                error_text: None,
                result_summary: Some("started work".to_owned()),
                transcript_path: Some("/tmp/transcript.jsonl".to_owned()),
                artifacts_path: Some("/tmp/artifacts".to_owned()),
                started_at: Some("100".to_owned()),
                finished_at: None,
            })
            .unwrap();
        let attention = db
            .create_attention_item(CreateAttentionItemInput {
                execution_id: execution.id.clone(),
                kind: "decision_required".to_owned(),
                status: Some("open".to_owned()),
                title: "Need product call".to_owned(),
                body_markdown: "Please decide.".to_owned(),
                resolved_at: None,
            })
            .unwrap();

        let executions = db.list_executions(Some(&task.id)).unwrap();
        assert_eq!(executions.len(), 1);
        assert_eq!(executions[0].id, execution.id);
        assert_eq!(db.get_execution(&execution.id).unwrap().id, execution.id);

        let runs = db.list_runs(&execution.id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, run.id);
        assert_eq!(
            db.get_run(&run.id).unwrap().transcript_path.as_deref(),
            Some("/tmp/transcript.jsonl")
        );

        let items = db.list_attention_items(&execution.id).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, attention.id);
        assert_eq!(
            db.get_attention_item(&attention.id).unwrap().title,
            "Need product call"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn execution_requires_repo_remote_url_snapshot() {
        let path = temp_db_path("execution-repo");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();

        let err = db
            .create_execution(CreateExecutionInput {
                work_item_id: product.id.clone(),
                kind: "project_design".to_owned(),
                status: None,
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("does not resolve to a product repo_remote_url")
        );

        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: product.id.clone(),
                kind: "project_design".to_owned(),
                status: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();
        assert_eq!(
            execution.repo_remote_url,
            "git@github.com:spinyfin/mono.git"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reconciles_missing_executions_for_product_tree() {
        let path = temp_db_path("reconcile-tree");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Execution coordinator".to_owned(),
                description: None,
                goal: None,
                autostart: true,
            })
            .unwrap();
        let first_task = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "First".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let second_task = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "Second".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();

        // Mark the project's auto-created design task done so the
        // first user-created project_task takes the head of the
        // dispatch chain — this test predates design-as-task and is
        // testing the project_task ordering, not the design phase.
        complete_design_for_project(&db, &project.id);

        let result = db.reconcile_product_executions(&product.id).unwrap();
        // Created executions: design (will reuse the existing one
        // from the design task — actually design task is now `done`
        // so it won't get reconciled), first_task, second_task,
        // chore. Plus the design task already had status='todo' before
        // we marked done — the reconcile may have created an execution
        // for it. To avoid coupling, just assert the per-task shape.
        assert!(result.updated.is_empty());

        let first_execution = db.list_executions(Some(&first_task.id)).unwrap();
        assert_eq!(first_execution.len(), 1);
        assert_eq!(first_execution[0].kind, "task_implementation");
        assert_eq!(first_execution[0].status, "ready");

        let second_execution = db.list_executions(Some(&second_task.id)).unwrap();
        assert_eq!(second_execution.len(), 1);
        assert_eq!(second_execution[0].status, "waiting_dependency");

        let chore_execution = db.list_executions(Some(&chore.id)).unwrap();
        assert_eq!(chore_execution.len(), 1);
        assert_eq!(chore_execution[0].kind, "chore_implementation");
        assert_eq!(chore_execution[0].status, "ready");

        let second_pass = db.reconcile_product_executions(&product.id).unwrap();
        assert!(second_pass.created.is_empty());
        assert!(second_pass.updated.is_empty());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reconcile_promotes_next_project_task_when_previous_done() {
        let path = temp_db_path("reconcile-promote");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Execution coordinator".to_owned(),
                description: None,
                goal: None,
                autostart: true,
            })
            .unwrap();
        let first_task = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "First".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let second_task = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "Second".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();

        // Mark the auto-created design task done so first_task takes
        // the head of the project's dispatch chain — without this, the
        // design task would be `ready` and first_task would still be
        // `waiting_dependency` after we mark first_task done (it never
        // becomes `ready` to begin with).
        complete_design_for_project(&db, &project.id);

        db.reconcile_product_executions(&product.id).unwrap();
        db.update_work_item(
            &first_task.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        let result = db.reconcile_product_executions(&product.id).unwrap();
        assert!(result.created.is_empty());
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].work_item_id, second_task.id);
        assert_eq!(result.updated[0].status, "ready");

        let second_execution = db.list_executions(Some(&second_task.id)).unwrap();
        assert_eq!(second_execution[0].status, "ready");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reconcile_waits_for_product_repo_remote_url() {
        let path = temp_db_path("reconcile-repo");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Execution coordinator".to_owned(),
                description: None,
                goal: None,
                autostart: true,
            })
            .unwrap();
        let task = db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "First".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();

        // Mark the auto-created design task done so the user's task
        // is the first incomplete and would be `ready` once the repo
        // remote becomes available — that's what this test cares
        // about.
        complete_design_for_project(&db, &project.id);

        let first_pass = db.reconcile_product_executions(&product.id).unwrap();
        assert!(first_pass.created.is_empty());
        assert!(db.list_executions(None).unwrap().is_empty());

        db.update_work_item(
            &product.id,
            WorkItemPatch {
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let second_pass = db.reconcile_product_executions(&product.id).unwrap();
        // Now there's exactly one executable item under the project
        // (the user task; the design is `done`), so reconcile creates
        // exactly one execution.
        assert_eq!(second_pass.created.len(), 1);

        let task_execution = db.list_executions(Some(&task.id)).unwrap();
        assert_eq!(task_execution.len(), 1);
        assert_eq!(task_execution[0].status, "ready");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn starts_ready_execution_run_and_attaches_workspace() {
        let path = temp_db_path("start-run");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();

        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                "/tmp/mono-agent-001",
            )
            .unwrap();
        assert_eq!(execution.status, "running");
        assert_eq!(execution.cube_repo_id.as_deref(), Some("mono"));
        assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
        assert_eq!(
            execution.workspace_path.as_deref(),
            Some("/tmp/mono-agent-001")
        );
        assert!(execution.started_at.is_some());
        assert_eq!(run.execution_id, execution.id);
        assert_eq!(run.agent_id, "worker-1");
        assert_eq!(run.status, "active");
        assert!(run.started_at.is_some());
        assert!(run.finished_at.is_none());

        // Auto-advance: starting an execution moves the chore's
        // kanban status from `todo` to `active` so it appears in the
        // Doing column.
        let advanced_chore = db.get_work_item(&chore.id).unwrap();
        match advanced_chore {
            WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, "active"),
            other => panic!("expected chore/task, got {other:?}"),
        }

        // Stamping the real pane slot back onto agent_id: the
        // coordinator calls this once SpawnWorkerPane responds with
        // the slot the app actually allocated. Looking it up
        // afterwards must reflect the new value.
        let updated = db.set_run_agent_id(&run.id, "worker-3").unwrap();
        assert_eq!(updated.agent_id, "worker-3");
        let reread = db.get_run(&run.id).unwrap();
        assert_eq!(reread.agent_id, "worker-3");

        let unknown = db.set_run_agent_id("run-does-not-exist", "worker-1");
        assert!(unknown.is_err());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cancel_execution_marks_row_and_resets_active_chore_to_todo() {
        let path = temp_db_path("cancel-active");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();
        // Drive the chore into the Doing column by starting the run —
        // this is the state cancel is supposed to undo.
        db.start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();
        match db.get_work_item(&chore.id).unwrap() {
            WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, "active"),
            other => panic!("expected chore/task, got {other:?}"),
        }

        let cancelled = db.cancel_execution(&execution.id).unwrap();
        assert_eq!(cancelled.status, "cancelled");
        assert!(cancelled.finished_at.is_some());

        // Active → todo so the kanban card returns to the To-Do lane.
        match db.get_work_item(&chore.id).unwrap() {
            WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, "todo"),
            other => panic!("expected chore/task, got {other:?}"),
        }

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cancel_execution_preserves_in_review_and_done_status() {
        let path = temp_db_path("cancel-preserve");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Has PR".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("running".to_owned()),
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();
        // The worker opened a PR before the human asked to cancel.
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("in_review".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();

        db.cancel_execution(&execution.id).unwrap();

        // `in_review` survives — the PR still exists; cancel only
        // tears down the worker session, not the PR.
        match db.get_work_item(&chore.id).unwrap() {
            WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, "in_review"),
            other => panic!("expected chore/task, got {other:?}"),
        }

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cancel_execution_errors_on_unknown_id() {
        let path = temp_db_path("cancel-unknown");
        let db = WorkDb::open(path.clone()).unwrap();
        let err = db
            .cancel_execution("exec_does_not_exist")
            .expect_err("cancelling an unknown execution must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown execution"),
            "expected unknown-execution message, got: {msg}",
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn start_execution_does_not_downgrade_done_chores() {
        let path = temp_db_path("no-downgrade");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Already done".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        // Manually mark the chore as done before starting execution.
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();

        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();

        db.start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();

        // The chore was already `done` — auto-advance must not
        // overwrite that with `active`. Manual transitions win.
        let unchanged = db.get_work_item(&chore.id).unwrap();
        match unchanged {
            WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, "done"),
            other => panic!("expected chore/task, got {other:?}"),
        }

        let _ = std::fs::remove_file(path);
    }

    /// Reconcile must redispatch an `active` chore that has no
    /// execution at all — that's the "card sitting in Doing with no
    /// worker" case after a crash where the user had dragged the card
    /// into Doing but the engine never got to dispatch.
    #[test]
    fn reconcile_dispatches_active_chore_with_no_execution() {
        let path = temp_db_path("reconcile-no-exec");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Stranded chore".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        // Manually flip to active, mimicking a kanban drag that
        // wrote tasks.status without ever dispatching.
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();

        let redispatched = db.reconcile_active_dispatch(|_| true).unwrap();
        assert_eq!(redispatched, vec![chore.id.clone()]);

        // A ready execution should now exist for the chore.
        let executions = db.list_executions(Some(&chore.id)).unwrap();
        assert_eq!(executions.len(), 1, "expected exactly one execution");
        assert_eq!(executions[0].status, "ready");

        let _ = std::fs::remove_file(path);
    }

    /// Reconcile must redispatch when the only existing execution is
    /// terminal (completed/failed/abandoned) — same "no live worker"
    /// signal as having no execution at all.
    #[test]
    fn reconcile_redispatches_when_latest_execution_is_terminal() {
        let path = temp_db_path("reconcile-terminal");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Bounced chore".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        // Create an existing execution and force it to a terminal
        // status so the reconcile sees "latest execution is terminal."
        db.create_execution(CreateExecutionInput {
            work_item_id: chore.id.clone(),
            kind: "chore_implementation".to_owned(),
            status: Some("failed".to_owned()),
            ..Default::default()
        })
        .unwrap();

        let redispatched = db.reconcile_active_dispatch(|_| true).unwrap();
        assert_eq!(redispatched, vec![chore.id.clone()]);

        let executions = db.list_executions(Some(&chore.id)).unwrap();
        assert_eq!(
            executions.len(),
            2,
            "expected the failed exec plus a fresh ready one"
        );
        let latest = executions.last().unwrap();
        assert_eq!(latest.status, "ready");
    }

    /// Reconcile must NOT redispatch when a non-terminal execution
    /// already exists — that's a worker either currently running or
    /// queued, and re-dispatching would create a duplicate.
    #[test]
    fn reconcile_skips_active_chore_with_live_execution() {
        let path = temp_db_path("reconcile-live");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Live chore".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        // A waiting_human execution counts as non-terminal — worker
        // is paused but the slot is still owned.
        db.create_execution(CreateExecutionInput {
            work_item_id: chore.id.clone(),
            kind: "chore_implementation".to_owned(),
            status: Some("waiting_human".to_owned()),
            ..Default::default()
        })
        .unwrap();

        let redispatched = db.reconcile_active_dispatch(|_| true).unwrap();
        assert!(
            redispatched.is_empty(),
            "should not redispatch when a non-terminal execution exists, got {redispatched:?}",
        );
        let executions = db.list_executions(Some(&chore.id)).unwrap();
        assert_eq!(executions.len(), 1, "no fresh execution should be inserted");
    }

    /// Reconcile must redispatch when the latest execution is
    /// non-terminal on paper but the live-worker oracle reports the
    /// slot is gone. This is the "stale waiting_human after a crash"
    /// shape the design's §3 calls out — the row is non-terminal, no
    /// worker is actually attached, and a kanban drag would silently
    /// no-op without this carve-out.
    #[test]
    fn reconcile_redispatches_when_non_terminal_but_no_live_worker() {
        let path = temp_db_path("reconcile-stale");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Stale chore".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let stale = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("waiting_human".to_owned()),
                ..Default::default()
            })
            .unwrap();

        // is_live=false → reconcile should treat the waiting_human
        // row as stale, mark it abandoned, and insert a fresh ready.
        let redispatched = db.reconcile_active_dispatch(|_| false).unwrap();
        assert_eq!(redispatched, vec![chore.id.clone()]);

        let executions = db.list_executions(Some(&chore.id)).unwrap();
        assert_eq!(executions.len(), 2);
        let stale_after = executions
            .iter()
            .find(|e| e.id == stale.id)
            .expect("original stale exec should still be visible");
        assert_eq!(stale_after.status, "abandoned");
        let latest = executions.last().unwrap();
        assert_ne!(latest.id, stale.id);
        assert_eq!(latest.status, "ready");
    }

    /// `list_in_flight_executions` is the input to the engine-startup
    /// reconciler: it returns rows that the engine considers actively
    /// occupying a worker (non-terminal status AND a recorded cube
    /// lease). This test pins the filter so the probe doesn't get
    /// dragged into rows it can't classify (terminal rows, ready rows
    /// without a lease, etc.).
    #[test]
    fn list_in_flight_executions_filters_by_status_and_lease() {
        let path = temp_db_path("list-in-flight");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();

        // (a) Running execution with a cube lease — the canonical
        //     in-flight row. Created via `start_execution_run` so the
        //     lease columns and status flip together exactly the way
        //     the dispatcher does it in production.
        let chore_a = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Active worker".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let exec_a = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore_a.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                ..Default::default()
            })
            .unwrap();
        db.start_execution_run(
            &exec_a.id,
            "worker-1",
            "mono",
            "lease-A",
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();

        // (b) Non-terminal but never claimed — no lease was ever
        //     recorded. Must NOT appear: the probe has nothing to
        //     match against cube state, and the existing ghost-active
        //     sweep handles never-dispatched ready rows.
        let chore_b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Stuck in queue".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        db.create_execution(CreateExecutionInput {
            work_item_id: chore_b.id.clone(),
            kind: "chore_implementation".to_owned(),
            status: Some("ready".to_owned()),
            ..Default::default()
        })
        .unwrap();

        // (c) Terminal status — must NOT appear regardless of lease.
        let chore_c = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Already done".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        db.create_execution(CreateExecutionInput {
            work_item_id: chore_c.id.clone(),
            kind: "chore_implementation".to_owned(),
            status: Some("completed".to_owned()),
            cube_lease_id: Some("lease-C".to_owned()),
            cube_workspace_id: Some("mono-agent-002".to_owned()),
            workspace_path: Some("/tmp/mono-agent-002".to_owned()),
            ..Default::default()
        })
        .unwrap();

        let in_flight = db.list_in_flight_executions().unwrap();
        let ids: Vec<&str> = in_flight.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec![exec_a.id.as_str()]);
        let row = &in_flight[0];
        assert_eq!(row.cube_lease_id.as_deref(), Some("lease-A"));
        assert_eq!(row.cube_workspace_id.as_deref(), Some("mono-agent-001"));

        let _ = std::fs::remove_file(path);
    }

    /// End-to-end coverage for the engine-startup reconcile path with
    /// a mix of live, dead, and unknown persisted runs — the explicit
    /// acceptance test from the work-item brief. We exercise this at
    /// the `reconcile_active_dispatch` level (not the cube probe
    /// level, which has its own tests in `run_reconcile`) so we pin
    /// the contract: live + unknown rows are left intact, only dead
    /// rows are abandoned and redispatched.
    #[test]
    fn reconcile_with_mixed_verdicts_only_redispatches_dead_runs() {
        use std::collections::HashSet;
        let path = temp_db_path("reconcile-mixed");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();

        // Three chores, each with a `running` execution and a recorded
        // cube lease — exactly the shape `start_execution_run`
        // produces.  The engine restarts; the probe will classify
        // each row and feed `reconcile_active_dispatch`.
        let live = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Worker still up".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let exec_live = db
            .create_execution(CreateExecutionInput {
                work_item_id: live.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                ..Default::default()
            })
            .unwrap();
        db.start_execution_run(
            &exec_live.id,
            "worker-1",
            "mono",
            "lease-LIVE",
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();

        let dead = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Worker died with engine".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let exec_dead = db
            .create_execution(CreateExecutionInput {
                work_item_id: dead.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                ..Default::default()
            })
            .unwrap();
        db.start_execution_run(
            &exec_dead.id,
            "worker-2",
            "mono",
            "lease-DEAD",
            "mono-agent-002",
            "/tmp/mono-agent-002",
        )
        .unwrap();

        let unknown = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cube didn't know".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let exec_unknown = db
            .create_execution(CreateExecutionInput {
                work_item_id: unknown.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                ..Default::default()
            })
            .unwrap();
        db.start_execution_run(
            &exec_unknown.id,
            "worker-3",
            "mono",
            "lease-UNK",
            "mono-agent-003",
            "/tmp/mono-agent-003",
        )
        .unwrap();

        // Simulate the cube probe's output: live + unknown stay in
        // the skip-dispatch set; dead falls out and gets redispatched.
        let skip_dispatch: HashSet<String> =
            [exec_live.id.clone(), exec_unknown.id.clone()].into_iter().collect();

        let redispatched = db
            .reconcile_active_dispatch(|execution_id| skip_dispatch.contains(execution_id))
            .unwrap();
        assert_eq!(
            redispatched,
            vec![dead.id.clone()],
            "only the dead run's work item should be redispatched",
        );

        // Live row: unchanged. Still `running` with the original lease.
        let live_after = db.list_executions(Some(&live.id)).unwrap();
        assert_eq!(live_after.len(), 1, "live execution row must be preserved");
        assert_eq!(live_after[0].status, "running");
        assert_eq!(live_after[0].cube_lease_id.as_deref(), Some("lease-LIVE"));

        // Unknown row: also unchanged. The probe didn't know either
        // way, so we MUST NOT redispatch — that's the conservatism
        // the work-item brief insists on ("ambiguous → leave alone").
        let unknown_after = db.list_executions(Some(&unknown.id)).unwrap();
        assert_eq!(unknown_after.len(), 1, "unknown execution row must be preserved");
        assert_eq!(unknown_after[0].status, "running");
        assert_eq!(unknown_after[0].cube_lease_id.as_deref(), Some("lease-UNK"));

        // Dead row: original abandoned, fresh `ready` row inserted
        // alongside it. The dispatcher will pick up the new row on
        // its next tick.
        let dead_after = db.list_executions(Some(&dead.id)).unwrap();
        assert_eq!(dead_after.len(), 2, "dead row gets a redispatch alongside the abandonment");
        let original = dead_after.iter().find(|e| e.id == exec_dead.id).unwrap();
        assert_eq!(original.status, "abandoned");
        assert!(original.finished_at.is_some());
        let fresh = dead_after.iter().find(|e| e.id != exec_dead.id).unwrap();
        assert_eq!(fresh.status, "ready");

        let _ = std::fs::remove_file(path);
    }

    /// Direct API test for the per-call live-aware path the kanban
    /// drag uses. Same shape as the reconcile-stale test but driven
    /// through `request_execution_with_live_check`.
    #[test]
    fn request_execution_marks_existing_stale_when_no_live_worker() {
        let path = temp_db_path("request-stale");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Stale chore".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let stale = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("waiting_human".to_owned()),
                ..Default::default()
            })
            .unwrap();

        let new_exec = db
            .request_execution_with_live_check(
                RequestExecutionInput {
                    work_item_id: chore.id.clone(),
                    priority: None,
                    preferred_workspace_id: None,
                    force: false,
                },
                |_| false,
            )
            .unwrap();

        assert_ne!(new_exec.id, stale.id, "expected a brand new execution row");
        assert_eq!(new_exec.status, "ready");

        let stale_after = db
            .list_executions(Some(&chore.id))
            .unwrap()
            .into_iter()
            .find(|e| e.id == stale.id)
            .unwrap();
        assert_eq!(stale_after.status, "abandoned");
        assert!(stale_after.finished_at.is_some());
    }

    /// And the inverse: live-worker → idempotent.
    #[test]
    fn request_execution_is_idempotent_when_existing_run_is_live() {
        let path = temp_db_path("request-live");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Live chore".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let live = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("waiting_human".to_owned()),
                ..Default::default()
            })
            .unwrap();

        let returned = db
            .request_execution_with_live_check(
                RequestExecutionInput {
                    work_item_id: chore.id.clone(),
                    priority: None,
                    preferred_workspace_id: None,
                    force: false,
                },
                |_| true,
            )
            .unwrap();

        assert_eq!(
            returned.id, live.id,
            "live worker → reuse existing execution",
        );
        assert_eq!(
            db.list_executions(Some(&chore.id)).unwrap().len(),
            1,
            "no duplicate execution should be inserted",
        );
    }

    /// Reconcile must NOT touch chores that aren't `active`. A
    /// `todo`/`done`/`archived` chore in the table is just data; the
    /// kanban Doing contract only applies to `active`.
    #[test]
    fn reconcile_ignores_non_active_chores() {
        let path = temp_db_path("reconcile-non-active");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let _todo_chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Stays in backlog".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let done_chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Already done".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        db.update_work_item(
            &done_chore.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();

        let redispatched = db.reconcile_active_dispatch(|_| true).unwrap();
        assert!(redispatched.is_empty());
    }

    /// `boss chore create --no-autostart` flips the
    /// `CreateChoreInput::autostart` flag off. A chore created that way
    /// is allowed to sit in `todo` without the engine spinning up a
    /// `ready` execution. Once a human (or `bossctl work start`) flips
    /// it to `active`, reconcile resumes normal behaviour and creates
    /// the execution.
    #[test]
    fn reconcile_skips_no_autostart_chore_until_status_changes() {
        let path = temp_db_path("no-autostart");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Parked".to_owned(),
                description: None,
                autostart: false,
                priority: None,
            })
            .unwrap();
        assert!(
            !chore.autostart,
            "create_chore must persist autostart=false"
        );

        // First reconcile right after create — must not create a
        // ready execution for the parked chore.
        let result = db.reconcile_product_executions(&product.id).unwrap();
        assert!(
            result.created.is_empty(),
            "no-autostart chore should not get a reconciled execution while it is in todo, got {:?}",
            result.created,
        );
        assert!(db.list_executions(Some(&chore.id)).unwrap().is_empty());

        // A second chore created normally MUST still be picked up by
        // reconcile (the no-autostart chore must not poison shared
        // reconcile state).
        let live = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Live".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let result = db.reconcile_product_executions(&product.id).unwrap();
        assert_eq!(result.created.len(), 1);
        assert_eq!(result.created[0].work_item_id, live.id);

        // Drag-to-Doing path: status flips to `active`, reconcile
        // resumes and creates the ready execution for the parked chore.
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let result = db.reconcile_product_executions(&product.id).unwrap();
        assert_eq!(result.created.len(), 1);
        assert_eq!(result.created[0].work_item_id, chore.id);
    }

    /// Steady-state on-free rescan: an `active` chore whose only
    /// execution is terminal (worker died, cube lease errored, …)
    /// gets a fresh `ready` row so the next `kick()` can land it on
    /// the freed worker. This is the path the create-time dispatcher
    /// alone can't reach — it only runs at chore creation, not on
    /// release.
    #[test]
    fn rescan_redispatches_active_chore_with_terminal_execution() {
        let path = temp_db_path("rescan-terminal");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Stuck chore".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        db.create_execution(CreateExecutionInput {
            work_item_id: chore.id.clone(),
            kind: "chore_implementation".to_owned(),
            status: Some("failed".to_owned()),
            ..Default::default()
        })
        .unwrap();

        let redispatched = db.rescan_active_dispatch().unwrap();
        assert_eq!(redispatched, vec![chore.id.clone()]);

        let executions = db.list_executions(Some(&chore.id)).unwrap();
        assert_eq!(executions.len(), 2);
        assert_eq!(executions.last().unwrap().status, "ready");
    }

    /// Same shape as the previous test, but the chore has no
    /// execution at all — the create-time dispatcher never inserted
    /// one, e.g. because a kanban drag set the status without going
    /// through `request_execution`. Rescan must still produce a
    /// `ready` row.
    #[test]
    fn rescan_redispatches_active_chore_with_no_execution() {
        let path = temp_db_path("rescan-no-exec");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Pristine chore".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();

        let redispatched = db.rescan_active_dispatch().unwrap();
        assert_eq!(redispatched, vec![chore.id.clone()]);
        let executions = db.list_executions(Some(&chore.id)).unwrap();
        assert_eq!(executions.len(), 1);
        assert_eq!(executions[0].status, "ready");
    }

    /// Rescan must NOT touch a chore whose latest execution is still
    /// non-terminal — that's an in-flight or already-queued worker.
    /// Re-dispatching would create a duplicate row that the scheduler
    /// would race against.
    #[test]
    fn rescan_skips_active_chore_with_live_execution() {
        let path = temp_db_path("rescan-live");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Live chore".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        db.create_execution(CreateExecutionInput {
            work_item_id: chore.id.clone(),
            kind: "chore_implementation".to_owned(),
            status: Some("ready".to_owned()),
            ..Default::default()
        })
        .unwrap();

        let redispatched = db.rescan_active_dispatch().unwrap();
        assert!(
            redispatched.is_empty(),
            "non-terminal execution should be left alone, got {redispatched:?}",
        );
        let executions = db.list_executions(Some(&chore.id)).unwrap();
        assert_eq!(executions.len(), 1, "no duplicate ready row should be inserted");
    }

    /// `autostart=false` items live in `active` deliberately: the
    /// human moved them there but explicitly opted out of the
    /// auto-dispatcher. The on-free rescan must respect that flag —
    /// `create_project` auto-spawns a `kind = 'design'` task at
    /// `ordinal = 0`, and reconcile dispatches it as a
    /// `project_design` execution. This is the join point that makes
    /// the project's design phase show up on the kanban as a
    /// regular task card.
    #[test]
    fn create_project_spawns_design_task_dispatched_as_project_design() {
        let path = temp_db_path("project-spawns-design");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Engine dispatch instrumentation".to_owned(),
                description: None,
                goal: Some("expose every dispatch event".to_owned()),
                autostart: true,
            })
            .unwrap();

        // The project comes with a `kind = 'design'` task already
        // attached, named "Design" and parked at `ordinal = 0` so it
        // sorts first in the project's chain.
        let tasks = db.list_tasks(&product.id, Some(&project.id), None).unwrap();
        let design = tasks
            .iter()
            .find(|t| t.kind == "design")
            .expect("project should have an auto-created design task");
        assert_eq!(design.name, "Design");
        assert_eq!(design.status, "todo");
        assert_eq!(design.ordinal, Some(0));
        assert_eq!(design.project_id.as_deref(), Some(project.id.as_str()));

        // Reconcile lights up the design task as a `project_design`
        // execution — same machinery as chore/task dispatch, just a
        // different kind on the work_executions row.
        db.reconcile_product_executions(&product.id).unwrap();
        let executions = db.list_executions(Some(&design.id)).unwrap();
        assert_eq!(executions.len(), 1);
        assert_eq!(executions[0].kind, "project_design");
        assert_eq!(executions[0].status, "ready");

        // The matching task runtime is in the work tree — that's
        // what the kanban joins to render the activity dot. No
        // separate "project runtime" needed.
        let tree = db.get_work_tree(&product.id).unwrap();
        let runtime = tree
            .task_runtimes
            .iter()
            .find(|r| r.work_item_id == design.id)
            .expect("design task runtime missing from work tree");
        assert_eq!(runtime.execution_status.as_deref(), Some("ready"));

        let _ = std::fs::remove_file(path);
    }

    /// `--no-autostart` on project create plumbs through to the
    /// design task's autostart flag — so the design lives in `todo`
    /// without spawning a worker until something explicitly schedules
    /// it. Mirrors the chore/task autostart story exactly.
    #[test]
    fn create_project_no_autostart_parks_design_task() {
        let path = temp_db_path("project-no-autostart-design");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let project = db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Parked".to_owned(),
                description: None,
                goal: None,
                autostart: false,
            })
            .unwrap();

        let tasks = db.list_tasks(&product.id, Some(&project.id), None).unwrap();
        let design = tasks
            .iter()
            .find(|t| t.kind == "design")
            .expect("project should have an auto-created design task");
        assert!(!design.autostart);

        // Reconcile must NOT create an execution for the parked
        // design task — the autostart gate keeps the dispatcher out.
        db.reconcile_product_executions(&product.id).unwrap();
        let executions = db.list_executions(Some(&design.id)).unwrap();
        assert!(
            executions.is_empty(),
            "no_autostart design task must NOT spawn an execution, found: {executions:?}",
        );

        let _ = std::fs::remove_file(path);
    }

    /// Pre-design-card databases don't have a design task per
    /// project. The migration fills the gap so the kanban renders
    /// existing projects the same way as new ones — a "Design"
    /// card sits at the head of each project's chain on next open.
    #[test]
    fn migration_backfills_design_tasks_for_existing_projects() {
        let path = temp_db_path("migration-design-backfill");
        // First open establishes the schema. We then forcibly delete
        // the auto-created design task to mirror a database created
        // before this column existed.
        let project_id = {
            let db = WorkDb::open(path.clone()).unwrap();
            let product = db
                .create_product(CreateProductInput {
                    name: "Boss".to_owned(),
                    description: None,
                    repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
                })
                .unwrap();
            let project = db
                .create_project(CreateProjectInput {
                    product_id: product.id,
                    name: "Legacy".to_owned(),
                    description: None,
                    goal: None,
                    autostart: true,
                })
                .unwrap();
            // Hard-delete the auto-created design task so the
            // database looks like a pre-migration row set on next
            // open.
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute(
                "DELETE FROM tasks WHERE project_id = ?1 AND kind = 'design'",
                params![project.id],
            )
            .unwrap();
            project.id
        };

        // Re-open: the migration fires and backfills a design task.
        let db = WorkDb::open(path.clone()).unwrap();
        let project = db.get_project(&project_id).unwrap();
        let tasks = db.list_tasks(&project.product_id, Some(&project_id), None).unwrap();
        let design = tasks
            .iter()
            .find(|t| t.kind == "design")
            .expect("migration should backfill a design task");
        assert_eq!(design.status, "todo");
        // Backfilled design tasks land parked: an existing project
        // may already be mid-flight under the old project-id-keyed
        // execution, so we don't auto-dispatch.
        assert!(!design.autostart);

        let _ = std::fs::remove_file(path);
    }

    /// otherwise a chore that died once would loop on every worker
    /// release.
    #[test]
    fn rescan_skips_no_autostart_active_chore() {
        let path = temp_db_path("rescan-no-autostart");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Parked".to_owned(),
                description: None,
                autostart: false,
                priority: None,
            })
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        db.create_execution(CreateExecutionInput {
            work_item_id: chore.id.clone(),
            kind: "chore_implementation".to_owned(),
            status: Some("failed".to_owned()),
            ..Default::default()
        })
        .unwrap();

        let redispatched = db.rescan_active_dispatch().unwrap();
        assert!(
            redispatched.is_empty(),
            "autostart=false items must stay parked, got {redispatched:?}",
        );
        let executions = db.list_executions(Some(&chore.id)).unwrap();
        assert_eq!(executions.len(), 1, "no fresh ready row for autostart=false");
        assert_eq!(executions[0].status, "failed");
    }

    /// FIFO ordering: the active chore that was moved to `active`
    /// first should be the first one redispatched. Later kanban
    /// drags wait their turn.
    #[test]
    fn rescan_orders_candidates_by_updated_at_ascending() {
        let path = temp_db_path("rescan-fifo");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let mut chore_ids = Vec::new();
        for index in 0..3 {
            let chore = db
                .create_chore(CreateChoreInput {
                    product_id: product.id.clone(),
                    name: format!("Chore {index}"),
                    description: None,
                    autostart: true,
                    priority: None,
                })
                .unwrap();
            chore_ids.push(chore.id);
        }

        // Drive `updated_at` to a known order: chore[0] dragged first,
        // chore[1] second, chore[2] last. The `updated_at` column has
        // second-level resolution, so write distinct stamps directly
        // to make the FIFO ordering deterministic without sleeping.
        for (index, chore_id) in chore_ids.iter().enumerate() {
            db.update_work_item(
                chore_id,
                WorkItemPatch {
                    status: Some("active".to_owned()),
                    ..Default::default()
                },
            )
            .unwrap();
            // Stamp updated_at to force the ordering. Earlier index =
            // earlier stamp = should rescan first.
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute(
                "UPDATE tasks SET updated_at = ?1 WHERE id = ?2",
                rusqlite::params![format!("2026-01-0{}T00:00:00Z", index + 1), chore_id],
            )
            .unwrap();
        }

        let redispatched = db.rescan_active_dispatch().unwrap();
        assert_eq!(
            redispatched, chore_ids,
            "rescan must redispatch in updated_at ASC order",
        );
    }

    /// Gated items (an unmet `blocks` prereq) must be silently
    /// skipped — bailing the transaction would drop redispatches
    /// for every later candidate too.
    #[test]
    fn rescan_skips_gated_active_chore_silently() {
        let path = temp_db_path("rescan-gated");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let prereq = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Prereq".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let dependent = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Dependent".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        // Add the blocks edge BEFORE flipping dependent to active so
        // its kanban transition lands on the gated path. We then set
        // status='active' directly via SQL to mimic state observed in
        // the bug — a row stuck in active without a healthy execution.
        db.add_dependency(AddDependencyInput {
            dependent: dependent.id.clone(),
            prerequisite: prereq.id.clone(),
            relation: None,
        })
        .unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'active' WHERE id = ?1",
            rusqlite::params![dependent.id],
        )
        .unwrap();
        drop(conn);

        let redispatched = db.rescan_active_dispatch().unwrap();
        assert!(
            !redispatched.contains(&dependent.id),
            "gated dependent must not be redispatched, got {redispatched:?}",
        );
        // The dependent has no fresh ready row — its only execution
        // (if any) is the gated one, and rescan didn't insert another.
        let dependent_execs = db.list_executions(Some(&dependent.id)).unwrap();
        assert!(
            dependent_execs
                .iter()
                .all(|exec| exec.status != "ready"),
            "no ready exec should be created for the gated dependent, got {dependent_execs:?}",
        );
    }

    #[test]
    fn records_failed_execution_start_attempt() {
        let path = temp_db_path("fail-run");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();

        let (execution, run) = db
            .fail_execution_start(
                &execution.id,
                "worker-1",
                Some("mono"),
                "cube workspace lease failed",
            )
            .unwrap();
        assert_eq!(execution.status, "failed");
        assert_eq!(execution.cube_repo_id.as_deref(), Some("mono"));
        assert!(execution.cube_lease_id.is_none());
        assert!(execution.workspace_path.is_none());
        assert!(execution.finished_at.is_some());
        assert_eq!(run.status, "failed");
        assert_eq!(
            run.error_text.as_deref(),
            Some("cube workspace lease failed")
        );
        assert!(run.finished_at.is_some());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn finishes_active_run_into_waiting_human_with_attention() {
        let path = temp_db_path("finish-run-waiting");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();

        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                "/tmp/mono-agent-001",
            )
            .unwrap();

        let (execution, run, attention) = db
            .finish_execution_run(
                &execution.id,
                &run.id,
                "waiting_human",
                "completed",
                Some("Implemented the first pass."),
                None,
                false,
                Some(CreateAttentionItemInput {
                    execution_id: execution.id.clone(),
                    kind: "review_required".to_owned(),
                    status: None,
                    title: "Review implementation output for Cleanup".to_owned(),
                    body_markdown: "Review requested.".to_owned(),
                    resolved_at: None,
                }),
            )
            .unwrap();

        assert_eq!(execution.status, "waiting_human");
        assert_eq!(execution.cube_lease_id.as_deref(), Some("lease-1"));
        assert_eq!(
            execution.workspace_path.as_deref(),
            Some("/tmp/mono-agent-001")
        );
        assert!(execution.finished_at.is_none());
        assert_eq!(run.status, "completed");
        assert_eq!(
            run.result_summary.as_deref(),
            Some("Implemented the first pass.")
        );
        assert!(run.error_text.is_none());
        assert!(run.finished_at.is_some());
        let attention = attention.expect("attention item should be created");
        assert_eq!(attention.kind, "review_required");
        assert_eq!(db.list_attention_items(&execution.id).unwrap().len(), 1);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn finishes_active_run_as_failed_and_clears_workspace_when_requested() {
        let path = temp_db_path("finish-run-failed");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Cleanup".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let execution = db
            .create_execution(CreateExecutionInput {
                work_item_id: chore.id.clone(),
                kind: "chore_implementation".to_owned(),
                status: Some("ready".to_owned()),
                repo_remote_url: None,
                cube_repo_id: None,
                cube_lease_id: None,
                cube_workspace_id: None,
                workspace_path: None,
                priority: None,
                preferred_workspace_id: None,
                started_at: None,
                finished_at: None,
            })
            .unwrap();

        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "mono-agent-001",
                "/tmp/mono-agent-001",
            )
            .unwrap();

        let (execution, run, attention) = db
            .finish_execution_run(
                &execution.id,
                &run.id,
                "failed",
                "failed",
                None,
                Some("agent run failed"),
                true,
                None,
            )
            .unwrap();

        assert_eq!(execution.status, "failed");
        assert_eq!(execution.cube_repo_id.as_deref(), Some("mono"));
        assert!(execution.cube_lease_id.is_none());
        assert!(execution.workspace_path.is_none());
        assert!(execution.finished_at.is_some());
        assert_eq!(run.status, "failed");
        assert_eq!(run.error_text.as_deref(), Some("agent run failed"));
        assert!(attention.is_none());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn parse_iso8601_to_epoch_handles_canonical_shapes() {
        // Reference epochs cross-checked with `date -u -d '...' +%s`.
        assert_eq!(parse_iso8601_to_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso8601_to_epoch("2026-05-07T18:55:45Z"),
            Some(1_778_180_145)
        );
        // Fractional seconds are truncated, not rounded.
        assert_eq!(
            parse_iso8601_to_epoch("2026-05-07T18:55:45.000Z"),
            Some(1_778_180_145)
        );
        assert_eq!(
            parse_iso8601_to_epoch("2026-05-07T18:55:45.999Z"),
            Some(1_778_180_145)
        );
        // Already-canonical numeric strings are left untouched.
        assert_eq!(parse_iso8601_to_epoch("1778180145"), None);
        assert_eq!(parse_iso8601_to_epoch(""), None);
        // Non-UTC suffixes aren't supported (engine only writes Z).
        assert_eq!(parse_iso8601_to_epoch("2026-05-07T18:55:45+00:00"), None);
        // Malformed values fall through.
        assert_eq!(parse_iso8601_to_epoch("2026/05/07T18:55:45Z"), None);
        assert_eq!(parse_iso8601_to_epoch("2026-13-07T18:55:45Z"), None);
    }

    #[test]
    fn migrate_timestamps_rewrites_iso_rows_to_epoch() {
        let path = temp_db_path("ts-migrate");
        let db = WorkDb::open(path.clone()).unwrap();

        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: None,
            })
            .unwrap();
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "ISO chore".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();

        // Hand-roll an ISO 8601 timestamp into the row to mimic the
        // pre-canonical write path that produced the mixed format.
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET updated_at = ?1 WHERE id = ?2",
                params!["2026-05-07T18:55:45.000Z", chore.id],
            )
            .unwrap();
        }

        // Re-opening runs `init` -> `migrate_timestamps_to_epoch`.
        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();
        let updated_at: String = conn
            .query_row(
                "SELECT updated_at FROM tasks WHERE id = ?1",
                params![chore.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(updated_at, "1778180145");

        let _ = std::fs::remove_file(path);
    }

    /// Smoke test for the new dependency CRUD path. Adds an edge,
    /// re-adds it (idempotent), lists in both directions, then drops
    /// it. Cycles and self-loops are rejected at the engine boundary.
    #[test]
    fn dependency_add_list_and_remove_round_trip() {
        let path = temp_db_path("deps-crud");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let a = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "A".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();

        let edge = db
            .add_dependency(AddDependencyInput {
                dependent: a.id.clone(),
                prerequisite: b.id.clone(),
                relation: None,
            })
            .unwrap();
        assert_eq!(edge.dependent_id, a.id);
        assert_eq!(edge.prerequisite_id, b.id);
        assert_eq!(edge.relation, "blocks");

        // Idempotent re-add: same edge, no error, no duplicate row.
        let edge2 = db
            .add_dependency(AddDependencyInput {
                dependent: a.id.clone(),
                prerequisite: b.id.clone(),
                relation: Some("blocks".to_owned()),
            })
            .unwrap();
        assert_eq!(edge2, edge);

        // Cycle: B → A would close the loop A → B → A.
        let cycle = db.add_dependency(AddDependencyInput {
            dependent: b.id.clone(),
            prerequisite: a.id.clone(),
            relation: None,
        });
        assert!(cycle.is_err(), "expected cycle rejection");
        assert!(cycle.unwrap_err().to_string().contains("cycle"));

        // Self-loop: rejected at the engine before hitting the schema.
        let self_loop = db.add_dependency(AddDependencyInput {
            dependent: a.id.clone(),
            prerequisite: a.id.clone(),
            relation: None,
        });
        assert!(self_loop.is_err());

        // List in both directions.
        let view = db
            .list_dependencies(ListDependenciesInput {
                work_item: a.id.clone(),
                direction: None,
            })
            .unwrap();
        assert_eq!(view.prerequisites.len(), 1);
        assert_eq!(view.prerequisites[0].prerequisite_id, b.id);
        assert!(view.dependents.is_empty());

        let view_b = db
            .list_dependencies(ListDependenciesInput {
                work_item: b.id.clone(),
                direction: None,
            })
            .unwrap();
        assert!(view_b.prerequisites.is_empty());
        assert_eq!(view_b.dependents.len(), 1);
        assert_eq!(view_b.dependents[0].dependent_id, a.id);

        // Remove returns true; second remove returns false (no error).
        let removed = db
            .remove_dependency(RemoveDependencyInput {
                dependent: a.id.clone(),
                prerequisite: b.id.clone(),
                relation: None,
            })
            .unwrap();
        assert!(removed);
        let removed_again = db
            .remove_dependency(RemoveDependencyInput {
                dependent: a.id.clone(),
                prerequisite: b.id.clone(),
                relation: None,
            })
            .unwrap();
        assert!(!removed_again);

        let _ = std::fs::remove_file(path);
    }

    /// Cross-product edges are refused at the engine boundary
    /// (Q3-iii — same-product, cross-project, cross-kind is the v1
    /// scope).
    #[test]
    fn dependency_add_refuses_cross_product_edges() {
        let path = temp_db_path("deps-cross-product");
        let db = WorkDb::open(path.clone()).unwrap();
        let p1 = db
            .create_product(CreateProductInput {
                name: "Alpha".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/alpha.git".to_owned()),
            })
            .unwrap();
        let p2 = db
            .create_product(CreateProductInput {
                name: "Beta".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/beta.git".to_owned()),
            })
            .unwrap();
        let a = db
            .create_chore(CreateChoreInput {
                product_id: p1.id,
                name: "Alpha task".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let b = db
            .create_chore(CreateChoreInput {
                product_id: p2.id,
                name: "Beta task".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let err = db
            .add_dependency(AddDependencyInput {
                dependent: a.id,
                prerequisite: b.id,
                relation: None,
            })
            .unwrap_err()
            .to_string();
        assert!(err.contains("cross-product"));
        assert!(err.contains("proj_18a2bbe20fc03718_8"));
        let _ = std::fs::remove_file(path);
    }

    /// Q10: deleting a prereq drops every edge that names it as
    /// either endpoint. The dependent's row stays where it is — its
    /// status will be reconciled on the next pass — but the gating
    /// relationship is cleared so it isn't stuck on a tombstone.
    #[test]
    fn deleting_a_task_drops_its_dependency_edges() {
        let path = temp_db_path("deps-delete-cascade");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            })
            .unwrap();
        let a = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "A".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        db.add_dependency(AddDependencyInput {
            dependent: a.id.clone(),
            prerequisite: b.id.clone(),
            relation: None,
        })
        .unwrap();
        db.delete_work_item(&b.id).unwrap();

        let view = db
            .list_dependencies(ListDependenciesInput {
                work_item: a.id.clone(),
                direction: None,
            })
            .unwrap();
        assert!(
            view.prerequisites.is_empty(),
            "expected dangling edge to be dropped on prereq delete"
        );
        let _ = std::fs::remove_file(path);
    }

    /// Adding an edge against an unsatisfied prereq flips the
    /// dependent to `blocked` and stamps `last_status_actor = engine`.
    /// Dropping the edge (with no remaining gating) unblocks it back
    /// to `todo`. Manual blocks (a human-set status) are not touched.
    #[test]
    fn auto_block_and_unblock_follow_edge_lifecycle() {
        let path = temp_db_path("deps-auto-block");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@example.com:boss.git".to_owned()),
            })
            .unwrap();
        let a = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "A".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        // Sanity: A starts as `todo` (default).
        let a0 = db.get_work_item(&a.id).unwrap();
        let WorkItem::Chore(t) = a0 else { panic!() };
        assert_eq!(t.status, "todo");

        // Adding A → B (B not satisfied) auto-blocks A.
        db.add_dependency(AddDependencyInput {
            dependent: a.id.clone(),
            prerequisite: b.id.clone(),
            relation: None,
        })
        .unwrap();
        let a1 = db.get_work_item(&a.id).unwrap();
        let WorkItem::Chore(t1) = a1 else { panic!() };
        assert_eq!(t1.status, "blocked");
        assert_eq!(t1.last_status_actor, "engine");

        // Removing the edge auto-unblocks A back to `todo`.
        db.remove_dependency(RemoveDependencyInput {
            dependent: a.id.clone(),
            prerequisite: b.id.clone(),
            relation: None,
        })
        .unwrap();
        let a2 = db.get_work_item(&a.id).unwrap();
        let WorkItem::Chore(t2) = a2 else { panic!() };
        assert_eq!(t2.status, "todo");
        let _ = std::fs::remove_file(path);
    }

    /// When the prereq's status flips to `done`, dependents on it
    /// auto-unblock if the engine put them in `blocked`. A manual
    /// block stays.
    #[test]
    fn dependent_auto_unblocks_when_prereq_marked_done() {
        let path = temp_db_path("deps-cascade-done");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@example.com:boss.git".to_owned()),
            })
            .unwrap();
        let a = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "A".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        db.add_dependency(AddDependencyInput {
            dependent: a.id.clone(),
            prerequisite: b.id.clone(),
            relation: None,
        })
        .unwrap();

        // Move B to `done` via UpdateWorkItem.
        db.update_work_item(
            &b.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        let a_after = db.get_work_item(&a.id).unwrap();
        let WorkItem::Chore(t) = a_after else {
            panic!()
        };
        assert_eq!(t.status, "todo");
        assert_eq!(t.last_status_actor, "engine");
        let _ = std::fs::remove_file(path);
    }

    /// A human-blocked dependent (no edges) is not touched by the
    /// auto-unblock path — the user has to clear it themselves.
    #[test]
    fn manual_block_is_not_auto_unblocked() {
        let path = temp_db_path("deps-manual-block");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@example.com:boss.git".to_owned()),
            })
            .unwrap();
        let a = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "A".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        // Human moves A to `blocked` (no edges yet).
        db.update_work_item(
            &a.id,
            WorkItemPatch {
                status: Some("blocked".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        let a1 = db.get_work_item(&a.id).unwrap();
        let WorkItem::Chore(t1) = a1 else { panic!() };
        assert_eq!(t1.status, "blocked");
        assert_eq!(t1.last_status_actor, "human");

        // Adding then removing an edge against an already-satisfied
        // prereq should not flip the manually-blocked row off
        // `blocked` (last_status_actor stays `human`).
        let b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        db.update_work_item(
            &b.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        db.add_dependency(AddDependencyInput {
            dependent: a.id.clone(),
            prerequisite: b.id.clone(),
            relation: None,
        })
        .unwrap();
        db.remove_dependency(RemoveDependencyInput {
            dependent: a.id.clone(),
            prerequisite: b.id.clone(),
            relation: None,
        })
        .unwrap();

        let a_after = db.get_work_item(&a.id).unwrap();
        let WorkItem::Chore(t) = a_after else {
            panic!()
        };
        assert_eq!(t.status, "blocked");
        assert_eq!(t.last_status_actor, "human");
        let _ = std::fs::remove_file(path);
    }

    /// A manual move from `blocked` to anything else is refused
    /// while gating prereqs remain (Q4 case 1).
    #[test]
    fn refuses_manual_move_off_blocked_while_gated() {
        let path = temp_db_path("deps-refuse-move");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@example.com:boss.git".to_owned()),
            })
            .unwrap();
        let a = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "A".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        db.add_dependency(AddDependencyInput {
            dependent: a.id.clone(),
            prerequisite: b.id.clone(),
            relation: None,
        })
        .unwrap();
        let err = db
            .update_work_item(
                &a.id,
                WorkItemPatch {
                    status: Some("active".to_owned()),
                    ..WorkItemPatch::default()
                },
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("gated by"), "unexpected error: {err}");
        let _ = std::fs::remove_file(path);
    }

    /// Reconcile downgrades a gated dependent's execution to
    /// `waiting_dependency` instead of `ready`. When the prereq
    /// completes, a follow-up reconcile promotes it back to `ready`.
    #[test]
    fn dispatcher_holds_gated_dependents_in_waiting_dependency() {
        let path = temp_db_path("deps-dispatcher");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@example.com:boss.git".to_owned()),
            })
            .unwrap();
        let a = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "A".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        db.add_dependency(AddDependencyInput {
            dependent: a.id.clone(),
            prerequisite: b.id.clone(),
            relation: None,
        })
        .unwrap();

        db.reconcile_product_executions(&product.id).unwrap();
        let exec_a = db.list_executions(Some(&a.id)).unwrap().pop().unwrap();
        assert_eq!(exec_a.status, "waiting_dependency");
        let exec_b = db.list_executions(Some(&b.id)).unwrap().pop().unwrap();
        assert_eq!(exec_b.status, "ready");

        // Move B to done. Reconcile then promotes A's execution to ready.
        db.update_work_item(
            &b.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        db.reconcile_product_executions(&product.id).unwrap();
        let exec_a_after = db.list_executions(Some(&a.id)).unwrap().pop().unwrap();
        assert_eq!(exec_a_after.status, "ready");
        let _ = std::fs::remove_file(path);
    }

    /// Explicit `RequestExecution` against a gated work item is
    /// refused (Q8) — the user removes the edge or waits for the
    /// prereq to complete.
    #[test]
    fn request_execution_refuses_gated_work_item() {
        let path = temp_db_path("deps-req-gated");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@example.com:boss.git".to_owned()),
            })
            .unwrap();
        let a = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "A".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        let b = db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "B".to_owned(),
                description: None,
                autostart: true,
                priority: None,
            })
            .unwrap();
        db.add_dependency(AddDependencyInput {
            dependent: a.id.clone(),
            prerequisite: b.id.clone(),
            relation: None,
        })
        .unwrap();
        let err = db
            .request_execution(RequestExecutionInput {
                work_item_id: a.id.clone(),
                priority: None,
                preferred_workspace_id: None,
                force: false,
            })
            .unwrap_err()
            .to_string();
        assert!(err.contains("gated by"), "unexpected error: {err}");
        let _ = std::fs::remove_file(path);
    }

    /// Pre-v3 / pre-v4 databases should pick up the new dependency
    /// table and `last_status_actor` columns transparently on open;
    /// the engine writes `schema_version = 4`.
    #[test]
    fn migration_from_pre_v4_adds_deps_table_and_actor_columns() {
        let path = temp_db_path("deps-migrate");
        // Stand up a minimal v3 schema: just `tasks`, `projects`,
        // `metadata`, no dep table, no last_status_actor.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE products (
                 id TEXT PRIMARY KEY, name TEXT NOT NULL, slug TEXT NOT NULL UNIQUE,
                 description TEXT NOT NULL DEFAULT '', repo_remote_url TEXT,
                 status TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
             CREATE TABLE projects (
                 id TEXT PRIMARY KEY, product_id TEXT NOT NULL, name TEXT NOT NULL,
                 slug TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
                 goal TEXT NOT NULL DEFAULT '', status TEXT NOT NULL,
                 priority TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
             CREATE TABLE tasks (
                 id TEXT PRIMARY KEY, product_id TEXT NOT NULL, project_id TEXT,
                 kind TEXT NOT NULL, name TEXT NOT NULL,
                 description TEXT NOT NULL DEFAULT '', status TEXT NOT NULL,
                 ordinal INTEGER, pr_url TEXT, deleted_at TEXT,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                 autostart INTEGER NOT NULL DEFAULT 1);
             INSERT INTO metadata(key, value) VALUES ('schema_version','3');",
        )
        .unwrap();
        drop(conn);

        let db = WorkDb::open(path.clone()).unwrap();
        let conn = db.connect().unwrap();
        // The new table exists.
        let exists: i64 = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master
                 WHERE type='table' AND name='work_item_dependencies')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1);
        assert!(table_has_column(&conn, "tasks", "last_status_actor").unwrap());
        assert!(table_has_column(&conn, "projects", "last_status_actor").unwrap());
        let version: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "4");
        let _ = std::fs::remove_file(path);
    }
}
